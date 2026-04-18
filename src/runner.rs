
use core::net::SocketAddr;

use alloc::{collections::btree_map::BTreeMap, format, string::String, vec::Vec};
use defmt::{error, warn};
use embassy_futures::select::{Either, select};
use embassy_net::tcp::{self, TcpSocket};
use embedded_io_async::{Write};

use crate::{CmdReceiver, InfoSender, InternalCmd, MsgSender, NatsAuthenticator, NatsInfoMsg, NatsMsg};

enum State {
    Disconnected,
    Connected,
}


pub struct Runner<'a, A: NatsAuthenticator> {
    auth: A,
    state: State,
    address: SocketAddr,
    socket: TcpSocket<'a>,

    info_watch: InfoSender<'a>,
    cmd_channel: CmdReceiver<'a>,

    sub_map: BTreeMap<usize, MsgSender<'a>>,
    framer: Framer,
}
impl<'a, A: NatsAuthenticator> Runner<'a, A> {
    pub(crate) fn new(
        auth: A,
        address: SocketAddr,
        socket: TcpSocket<'a>,
        info_watch: InfoSender<'a>,
        cmd_channel: CmdReceiver<'a>,
    ) -> Self {
        let state = State::Disconnected;
        Self {
            auth,
            state,
            address,
            socket,

            info_watch,
            cmd_channel,

            sub_map: BTreeMap::new(),
            framer: Framer::new(),
        }
    }
    async fn read(&mut self) -> Result<(), tcp::Error> {
        let mut byte = 0;
        self.socket.read(core::slice::from_mut(&mut byte)).await?;
        if let Some(frame) = self.framer.insert(byte) {
            match frame {
                Frame::Ping => self.socket.write_all("PONG\r\n".as_bytes()).await?,
                Frame::Info(info) => {
                    defmt::info!("connected to nats: {}", info.server_name);
                    self.info_watch.send(info);
                    let msg = format!(
                        "CONNECT {}\r\n",
                        self.auth.connect_msg()
                    );
                    self.socket.write_all(msg.as_bytes()).await?;
                },
                Frame::Err => {
                    self.state = State::Disconnected;
                },
                Frame::Ok => (),
                Frame::Msg(nats_msg) => {
                    if let Some(ch) = self.sub_map.get(&nats_msg.sid) {
                        ch.send(nats_msg).await;
                    } else {
                        defmt::error!("Receiving message with no endpoint");
                        // TODO unsub
                    }
                },
            }
        }
        Ok(())
    }
    async fn subscribe(&mut self, topic: String, channel: MsgSender<'a>) -> Result<(), tcp::Error> {
        let sid = self.sub_map.keys().enumerate().find(|(i, k)| i < k).map(|(i, _)| i).unwrap_or(0);
        self.sub_map.insert(sid, channel);

        let msg = format!("SUB {} {}\r\n", topic, sid);
        self.socket.write_all(msg.as_bytes()).await
    }
    async fn publish(&mut self, topic: String, data: Vec<u8>) -> Result<(), tcp::Error> {
        let str_header = format!("PUB {} {}\r\n", topic, data.len());

        let header = str_header.as_bytes();
        const END: &[u8] = b"\r\n";

        let mut packet = Vec::with_capacity(header.len() + data.len() + END.len());

        packet.extend_from_slice(header);
        packet.extend_from_slice(&data);
        packet.extend_from_slice(END);

        self.socket.write_all(&packet).await
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
            error!("socket error: {}", e);
            self.state = State::Disconnected;
        }; 
    }
    async fn try_connect(&mut self) {
        match self.socket.connect(self.address).await {
            Ok(()) => {
                self.framer = Framer::new();
                self.state = State::Connected;
            },
            Err(e) => error!("could not connect to nats: {}", e),
        }
    }
    pub async fn run(&mut self) -> ! {
        loop {
            match self.state {
                State::Connected => self.run_connected().await,
                State::Disconnected => self.try_connect().await,
            }
        }
    }
}
impl<'a, A: NatsAuthenticator> Drop for Runner<'a, A> {
    fn drop(&mut self) {
        self.socket.close();
    }
}


enum FramerState {
    Sync,
    Msg(usize, usize, String),
}

enum Frame {
    Ping,
    Info(NatsInfoMsg),
    Err,
    Ok,
    Msg(NatsMsg),
}

pub struct Framer {
    state: FramerState,
    magic_pos: usize,
    buffer: Vec<u8>,
}

impl Framer {
    fn new() -> Self {
        Self { state: FramerState::Sync, magic_pos: 0, buffer: Vec::new() }
    }
    fn insert(&mut self, byte: u8) -> Option<Frame> {
        const CARR_RETURN: [u8; 2] = *b"\r\n";

        self.buffer.push(byte);
        if byte == CARR_RETURN[self.magic_pos] {
            self.magic_pos += 1;
            if self.magic_pos == CARR_RETURN.len() {
                self.magic_pos = 0;
                // removing carriage return from line
                self.buffer.truncate(self.buffer.len().saturating_sub(2));
                return self.handle_frame();
            }
        } else {
            self.magic_pos = 0;
        }
        return None
    }
    fn handle_frame(&mut self) -> Option<Frame> {
        match &self.state {
            FramerState::Sync => self.parse_header(),
            FramerState::Msg(len, sid, topic) => self.sync_msg(*len, *sid, topic.clone()),
        }
    }
    fn parse_header(&mut self) -> Option<Frame> {
        let packet_str = String::from_utf8(core::mem::take(&mut self.buffer)).unwrap();
        let (cmd, msg) = packet_str.split_once(' ').unwrap_or((&packet_str.trim(), ""));
        match cmd {
            "PING" => {
                return Some(Frame::Ping)
            }
            "INFO" => {
                if let Ok(info) = serde_json::from_str::<NatsInfoMsg>(msg) {
                    return Some(Frame::Info(info))
                } else {
                    warn!("could not decode nats info");
                }
            }
            "-ERR" => {
                error!("nats disconnected ({})", msg);
                return Some(Frame::Err);
            }
            "+OK" => {
                return Some(Frame::Ok);
            },
            "MSG" => {
                let Some((topic, msg)) = msg.split_once(' ') else {
                    error!("nats msg header parsing error (1)");
                    return None;
                };
                let Some((sid, msg)) = msg.split_once(' ') else {
                    error!("nats msg header parsing error (2)");
                    return None;
                };
                let (_reply_to, len) = msg.split_once(' ').unwrap_or(("", msg));
                let Ok(sid) = sid.parse::<usize>() else {
                    error!("nats sid parsing error: '{}'", sid);
                    return None;
                };
                let Ok(len) = len.parse::<usize>() else {
                    error!("nats msg len parsing error: '{}'", len);
                    return None;
                };
                self.state = FramerState::Msg(len, sid, String::from(topic));
            }
            default => warn!("unknown nats cmd {}", default),
        }

        None
    }

    fn sync_msg(&mut self, len: usize, sid: usize, topic: String) -> Option<Frame> {
        if self.buffer.len() >= len {
            self.state = FramerState::Sync;
            Some(Frame::Msg(NatsMsg {
                sid,
                topic: topic,
                data: core::mem::take(&mut self.buffer)
            }))
        } else {
            None
        }
    }
}
