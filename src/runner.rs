
use core::net::SocketAddr;

use embassy_futures::select::{Either, select};
use embassy_net::tcp::{self, TcpSocket};
use embedded_io_async::{Write};

use crate::{DELIM, U32_MAX_STR_LEN, AUTH_MAX_STR_LEN, BytesBuf, CmdReceiver, InfoSender, InternalCmd, MsgSender, NatsAuthenticator, NatsConfig, NatsInfoMsg, NatsMsg, StrBuf};

enum State {
    Disconnected,
    Authenticating,
    Connected,
}

#[derive(defmt::Format)]
enum Error {
    Disconnected,
    Tcp,
    Capacity,
    Ser,
    Utf8,
}

impl From<tcp::Error> for Error {
    fn from(_value: tcp::Error) -> Self {
        Self::Tcp
    }
}

impl From<heapless::CapacityError> for Error {
    fn from(_value: heapless::CapacityError) -> Self {
        Self::Capacity
    }
}

impl From<serde_json_core::ser::Error> for Error {
    fn from(_value: serde_json_core::ser::Error) -> Self {
        Self::Ser
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

    subs: heapless::Vec<(usize, C::Topic, MsgSender<'a, C>), N>,
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

            subs: heapless::Vec::new(),
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

                    let auth_msg = serde_json_core::to_string::<_, AUTH_MAX_STR_LEN>(&self.auth)?;

                    self.state = State::Connected;
                    self.socket.write_all(b"CONNECT ").await?;
                    self.socket.write_all(auth_msg.as_bytes()).await?;
                    self.socket.write_all(&DELIM).await?;

                    // resubscribe to all existing subscriptions
                    let resubs: heapless::Vec<_, N> = self.subs.iter().map(|(sid, topic, _)| (*sid, topic.clone())).collect();
                    for (sid, topic) in resubs {
                        self.send_sub_msg(sid, topic).await?;
                    }
                },
                Frame::Err => {
                    self.disconnect().await;
                },
                Frame::Ok => (),
                Frame::Msg(nats_msg) => {
                    if let Some((_, _, ch)) = self.subs.iter().find(|(sid, _, _)| sid == &nats_msg.sid) {
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
    async fn send_sub_msg(&mut self, sid: usize, topic: C::Topic) -> Result<(), Error> {
        self.socket.write_all(b"SUB ").await?;
        self.socket.write_all(topic.as_str().as_bytes()).await?;
        self.socket.write_all(b" ").await?;
        self.socket.write_all(heapless::format!(U32_MAX_STR_LEN; "{}", sid).unwrap().as_bytes()).await?;
        self.socket.write_all(&DELIM).await?;
        Ok(())
    }
    async fn subscribe(&mut self, topic: C::Topic, channel: MsgSender<'a, C>) -> Result<(), Error> {
        let sid = self.subs.len();

        self.subs.push((sid, topic.clone(), channel)).map_err(|_| Error::Capacity)?;

        self.send_sub_msg(sid, topic).await
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
            Err(e) => defmt::error!("could not connect to nats: {}", defmt::Debug2Format(&e)),
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
        // Clear buffer on err or if state was sync
        if was_sync || !matches!(res, Ok(None)) {
            self.buffer.clear();
        }
        res
    }

    fn parse_header(&mut self) -> Result<Option<Frame<C>>, Error> {

        fn parse_msg_header<C: NatsConfig>(msg: &str) -> Result<FramerState<C>, Error> {
            let Some((topic, msg)) = msg.split_once(' ') else {
                defmt::error!("nats msg header parsing error (1)");
                return Ok(FramerState::Sync);
            };
            let Some((sid, msg)) = msg.split_once(' ') else {
                defmt::error!("nats msg header parsing error (2)");
                return Ok(FramerState::Sync);
            };
            let (_reply_to, len) = msg.split_once(' ').unwrap_or(("", msg));
            let Ok(sid) = sid.parse::<usize>() else {
                defmt::error!("nats sid parsing error: '{}'", sid);
                return Ok(FramerState::Sync);
            };
            let Ok(len) = len.parse::<usize>() else {
                defmt::error!("nats msg len parsing error: '{}'", len);
                return Ok(FramerState::Sync);
            };

            Ok(FramerState::Msg(len, sid, C::Topic::try_from_str(topic)?))
        }

        let packet_str = core::str::from_utf8(&self.buffer.as_bytes())?;
        let (cmd, msg) = packet_str.trim().split_once(' ').unwrap_or((&packet_str.trim(), ""));
        let res = match cmd {
            "PING" => {
                Some(Frame::Ping)
            }
            "INFO" => {
                if let Ok((info, _)) = serde_json_core::from_str::<NatsInfoMsg>(msg) {
                    Some(Frame::Info(info))
                } else {
                    defmt::warn!("could not decode nats info");
                    None
                }
            }
            "-ERR" => {
                defmt::error!("nats disconnected ({})", msg);
                Some(Frame::Err)
            }
            "+OK" => {
                Some(Frame::Ok)
            },
            "MSG" => {
                self.state = parse_msg_header(msg)?;
                None
            }
            default => {
                defmt::warn!("unknown nats cmd {}", default);
                None
            }
        };

        Ok(res)
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
