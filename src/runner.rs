
use core::net::SocketAddr;

use embassy_futures::select::{Either, select};
use embassy_net::tcp::{self, TcpSocket};
use embedded_io_async::{Write};

use crate::{BytesBuf, CmdReceiver, InfoSender, InternalCmd, MsgSender, NatsAuthenticator, NatsConfig, NatsInfoMsg, NatsMsg, StrBuf};

const DELIM: [u8; 2] = *b"\r\n";
const U32_MAX_STR_LEN: usize = 10;

enum State {
    Disconnected,
    Authenticating,
    Connected,
}

#[derive(defmt::Format)]
enum Error {
    Disconnected,
    Tcp(tcp::Error),
    Capacity,
    Ser(serde_json_core::ser::Error),
    Utf8,
}

impl From<tcp::Error> for Error {
    fn from(value: tcp::Error) -> Self {
        Self::Tcp(value)
    }
}

impl From<heapless::CapacityError> for Error {
    fn from(_value: heapless::CapacityError) -> Self {
        Self::Capacity
    }
}

impl From<serde_json_core::ser::Error> for Error {
    fn from(value: serde_json_core::ser::Error) -> Self {
        Self::Ser(value)
    }
}

impl From<core::str::Utf8Error> for Error {
    fn from(_value: core::str::Utf8Error) -> Self {
        Self::Utf8
    }
}

pub struct Runner<'a, C: NatsConfig, A: NatsAuthenticator, const N: usize> {
    auth: A,
    state: State,
    address: SocketAddr,
    socket: TcpSocket<'a>,

    info_watch: InfoSender<'a>,
    cmd_channel: CmdReceiver<'a, C>,

    sub_map: heapless::Vec<(usize, MsgSender<'a, C>), N>,
    framer: Framer<C>,
}
impl<'a, C: NatsConfig, A: NatsAuthenticator, const N: usize> Runner<'a, C, A, N> {
    pub(crate) fn new(
        auth: A,
        address: SocketAddr,
        socket: TcpSocket<'a>,
        info_watch: InfoSender<'a>,
        cmd_channel: CmdReceiver<'a, C>,
    ) -> Self {
        let state = State::Disconnected;
        Self {
            auth,
            state,
            address,
            socket,

            info_watch,
            cmd_channel,

            sub_map: heapless::Vec::new(),
            framer: Framer::new(),
        }
    }
    async fn disconnect(&mut self) {
        self.socket.abort();
        let _ = self.socket.flush().await;
        self.state = State::Disconnected;
    }
    async fn read(&mut self) -> Result<(), Error> {
        let mut byte = 0;
        let n = self.socket.read(core::slice::from_mut(&mut byte)).await?;
        if n == 0 {
            return Err(Error::Disconnected)
        }
        if let Some(frame) = self.framer.insert(byte)? {
            match frame {
                Frame::Ping => {
                    self.socket.write_all("PONG".as_bytes()).await?;
                    self.socket.write_all(&DELIM).await?;
                },
                Frame::Info(info) => {
                    defmt::info!("connected to nats: {}", info.server_name.as_str());
                    self.info_watch.send(info);

                    const AUTH_STR_MAX_LEN: usize = 200;
                    let auth_msg = serde_json_core::to_string::<_, AUTH_STR_MAX_LEN>(&self.auth)?;
                    let auth_msg = auth_msg.as_bytes();

                    self.state = State::Connected;
                    self.socket.write_all(b"CONNECT ").await?;
                    self.socket.write_all(auth_msg).await?;
                    self.socket.write_all(&DELIM).await?;
                },
                Frame::Err => {
                    self.disconnect().await;
                },
                Frame::Ok => (),
                Frame::Msg(nats_msg) => {
                    if let Some((_, ch)) = self.sub_map.iter().find(|(id, _)| id == &nats_msg.sid) {
                        ch.send(nats_msg).await;
                    } else {
                        defmt::error!("Receiving message with no endpoint, unsubscribing...");
                        self.socket.write_all(b"UNSUB ").await?;
                        self.socket.write_all(heapless::format!(U32_MAX_STR_LEN; "{}", nats_msg.sid).unwrap().as_bytes()).await?;
                        self.socket.write_all(&DELIM).await?;
                    }
                },
            }
        }
        Ok(())
    }
    async fn subscribe(&mut self, topic: C::Topic, channel: MsgSender<'a, C>) -> Result<(), Error> {
        let sid = self.sub_map.len();

        self.sub_map.push((sid, channel)).map_err(|_| Error::Capacity)?;

        self.socket.write_all(b"SUB ").await?;
        self.socket.write_all(topic.as_str().as_bytes()).await?;
        self.socket.write_all(b" ").await?;
        self.socket.write_all(heapless::format!(U32_MAX_STR_LEN; "{}", sid).unwrap().as_bytes()).await?;
        self.socket.write_all(&DELIM).await?;

        Ok(())
    }
    async fn publish(&mut self, topic: C::Topic, data: C::Msg) -> Result<(), Error> {
        let data = data.as_bytes();

        // Header
        self.socket.write_all(b"PUB ").await?;
        self.socket.write_all(topic.as_str().as_bytes()).await?;
        self.socket.write_all(b" ").await?;
        self.socket.write_all(heapless::format!(U32_MAX_STR_LEN; "{}", data.len()).unwrap().as_bytes()).await?;
        self.socket.write_all(&DELIM).await?;

        // Body
        self.socket.write_all(data).await?;
        self.socket.write_all(&DELIM).await?;

        Ok(())
    }
    async fn run_connected(&mut self) {
        if let Err(e) = match select(
            self.socket.wait_read_ready(), 
            self.cmd_channel.receive(), 
        ).await {
            Either::First(()) => self.read().await,
            Either::Second(cmd) => match cmd {
                InternalCmd::Sub(topic, ch) => self.subscribe(topic, ch).await,
                InternalCmd::Pub(topic, data) => self.publish(topic, data).await,
            },
        } {
            defmt::error!("nats error: {}", e);
            self.disconnect().await;
        }; 
    }
    async fn run_auth_step(&mut self) {
        if let Err(e) = self.read().await {
            defmt::error!("nats error: {}", e);
            self.disconnect().await;
        }
    }
    async fn try_connect(&mut self) {
        match self.socket.connect(self.address).await {
            Ok(()) => {
                self.framer = Framer::new();
                self.state = State::Authenticating;
            },
            Err(e) => defmt::error!("could not connect to nats: {}", e),
        }
    }
    pub async fn run(&mut self) -> ! {
        loop {
            match self.state {
                State::Connected => self.run_connected().await,
                State::Authenticating => self.run_auth_step().await,
                State::Disconnected => self.try_connect().await,
            }
        }
    }
}
impl<'a, C: NatsConfig, A: NatsAuthenticator, const N: usize> Drop for Runner<'a, C, A, N> {
    fn drop(&mut self) {
        self.socket.close();
    }
}


enum FramerState<C: NatsConfig> {
    Sync,
    Msg(usize, usize, C::Topic),
}

enum Frame<C: NatsConfig> {
    Ping,
    Info(NatsInfoMsg),
    Err,
    Ok,
    Msg(NatsMsg<C>),
}

pub struct Framer<C: NatsConfig> {
    state: FramerState<C>,
    magic_pos: usize,
    buffer: C::Buf,
}

impl<C: NatsConfig> Framer<C> {
    fn new() -> Self {
        Self { state: FramerState::Sync, magic_pos: 0, buffer: C::Buf::default() }
    }
    fn insert(&mut self, byte: u8) -> Result<Option<Frame<C>>, Error> {

        self.buffer.push(byte)?;
        loop {
            if byte == DELIM[self.magic_pos] {
                self.magic_pos += 1;
                if self.magic_pos == DELIM.len() {
                    self.magic_pos = 0;
                    return self.handle_frame();
                }
                break;
            } else if self.magic_pos > 0 {
                self.magic_pos = 0;
            } else {
                break;
            }
        }
        return Ok(None)
    }
    fn handle_frame(&mut self) -> Result<Option<Frame<C>>, Error> {
        let was_sync = matches!(self.state, FramerState::Sync);
        let res = match &self.state {
            FramerState::Sync => self.parse_header(),
            FramerState::Msg(len, sid, topic) => self.sync_msg(*len, *sid, topic.clone()),
        };
        // Clear buffer on err or if state is sync
        if was_sync || !matches!(res, Ok(None)) {
            self.buffer.clear();
        }
        res
    }
    fn parse_header(&mut self) -> Result<Option<Frame<C>>, Error> {
        let packet_str = core::str::from_utf8(&self.buffer.as_bytes())?;
        let (cmd, msg) = packet_str.trim().split_once(' ').unwrap_or((&packet_str.trim(), ""));
        match cmd {
            "PING" => {
                return Ok(Some(Frame::Ping))
            }
            "INFO" => {
                if let Ok((info, _)) = serde_json_core::from_str::<NatsInfoMsg>(msg) {
                    return Ok(Some(Frame::Info(info)))
                } else {
                    defmt::warn!("could not decode nats info");
                }
            }
            "-ERR" => {
                defmt::error!("nats disconnected ({})", msg);
                return Ok(Some(Frame::Err));
            }
            "+OK" => {
                return Ok(Some(Frame::Ok));
            },
            "MSG" => {
                let Some((topic, msg)) = msg.split_once(' ') else {
                    defmt::error!("nats msg header parsing error (1)");
                    return Ok(None);
                };
                let Some((sid, msg)) = msg.split_once(' ') else {
                    defmt::error!("nats msg header parsing error (2)");
                    return Ok(None);
                };
                let (_reply_to, len) = msg.split_once(' ').unwrap_or(("", msg));
                let Ok(sid) = sid.parse::<usize>() else {
                    defmt::error!("nats sid parsing error: '{}'", sid);
                    return Ok(None);
                };
                let Ok(len) = len.parse::<usize>() else {
                    defmt::error!("nats msg len parsing error: '{}'", len);
                    return Ok(None);
                };
                self.state = FramerState::Msg(len, sid, C::Topic::try_from_str(topic)?);
            }
            default => defmt::warn!("unknown nats cmd {}", default),
        }

        Ok(None)
    }

    fn sync_msg(&mut self, len: usize, sid: usize, topic: C::Topic) -> Result<Option<Frame<C>>, Error> {
        if self.buffer.len() >= len {
            self.state = FramerState::Sync;
            let data = C::Msg::try_from_bytes(&self.buffer.as_bytes()[..len])?;
            Ok(Some(Frame::Msg(NatsMsg {
                sid,
                topic,
                data,
            })))
        } else {
            Ok(None)
        }
    }
}
