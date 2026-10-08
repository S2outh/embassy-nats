use core::net::SocketAddr;

use embassy_futures::select::{Either, select};
use embassy_net::tcp::{self, TcpSocket};
use embedded_io_async::Write;
use thiserror::Error;

mod framer;
use framer::Frame;
use framer::Framer;

use crate::runner::framer::FramerError;
use crate::{
    AUTH_JSON_MAX_LEN, BytesBuf, CapacityError, CmdReceiver, DELIM, InfoSender, InternalCmd,
    MsgSender, NatsAuthenticator, NatsCollections, StrBuf, U32_MAX_STR_LEN,
};

macro_rules! defmt {
    ($($t:tt)*) => {{
        #[cfg(feature = "defmt")]
        defmt::$($t)*;
    }};
}

enum State {
    Disconnected,
    Authenticating,
    Connected,
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Error)]
enum Error {
    #[error("Disconnected")]
    Disconnected,
    #[error("Tcp error: {0}")]
    Tcp(#[from] tcp::Error),
    #[error("Capacity of collection {0} not sufficient for operation")]
    Capacity(#[from] CapacityError),
    #[error("Json serialization error {0}")]
    Ser(#[from] serde_json_core::ser::Error),
    #[error("Json deserialization error {0}")]
    Deser(#[from] serde_json_core::de::Error),
    #[error("Invalid utf8")]
    Utf8,
    #[error("Invalid header")]
    Header,
}

type Result<T> = core::result::Result<T, Error>;

impl From<FramerError<tcp::Error>> for Error {
    fn from(value: FramerError<tcp::Error>) -> Self {
        match value {
            FramerError::Disconnected => Error::Disconnected,
            FramerError::Read(e) => Error::Tcp(e),
            FramerError::Capacity(e) => Error::Capacity(e),
            FramerError::Deser(e) => Error::Deser(e),
            FramerError::Utf8 => Error::Utf8,
            FramerError::Header => Error::Header,
        }
    }
}

pub struct Runner<'a, C: NatsCollections, A: NatsAuthenticator, const N: usize> {
    auth: A,
    state: State,
    address: SocketAddr,
    socket: TcpSocket<'a>,

    info_watch: InfoSender<'a>,
    cmd_channel: CmdReceiver<'a, C>,

    subs: heapless::Vec<(usize, C::Topic, MsgSender<'a, C>, bool), N>,
    framer: Framer<C>,
}
impl<'a, C: NatsCollections, A: NatsAuthenticator, const N: usize> Runner<'a, C, A, N> {
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
    async fn read(&mut self) -> Result<()> {
        let Some(frame) = self.framer.frame(&mut self.socket).await? else {
            return Ok(());
        };
        match frame {
            Frame::Ping => {
                self.socket.write_all("PONG".as_bytes()).await?;
                self.socket.write_all(&DELIM).await?;
            }
            Frame::Info(info) => {
                defmt!(info!("connected to nats: {}", info.server_name.as_str()));
                self.info_watch.send(info);

                // serialize the connect message body into AuthBuf
                let mut connect_msg = C::AuthBuf::default();
                let connect_msg_slice = connect_msg.extend_by(AUTH_JSON_MAX_LEN)?;
                let len = serde_json_core::to_slice(&self.auth, connect_msg_slice)?;
                connect_msg.truncate(len);

                self.socket.write_all(b"CONNECT ").await?;
                self.socket.write_all(connect_msg.as_bytes()).await?;
                self.socket.write_all(&DELIM).await?;

                // resubscribe to all existing subscriptions
                for i in 0..self.subs.len() {
                    let (sid, topic, _, active) = &self.subs[i];
                    if *active {
                        self.subscribe(*sid, topic.clone()).await?;
                    }
                }

                // Update state
                self.state = State::Connected;
            }
            Frame::Err => {
                self.disconnect().await;
            }
            Frame::Ok => (),
            Frame::Msg(nats_msg) => {
                if let Some((sid, _, ch, active)) =
                    self.subs.iter().find(|(sid, _, _, _)| sid == &nats_msg.sid)
                {
                    if *active {
                        ch.send(nats_msg).await;
                    } else {
                        self.unsubscribe(*sid).await?;
                    }
                } else {
                    defmt!(error!(
                        "Receiving message with no endpoint, unsubscribing..."
                    ));
                    self.unsubscribe(nats_msg.sid).await?;
                }
            }
        }

        Ok(())
    }
    async fn subscribe(&mut self, sid: usize, topic: C::Topic) -> Result<()> {
        self.socket.write_all(b"SUB ").await?;
        self.socket.write_all(topic.as_str().as_bytes()).await?;
        self.socket.write_all(b" ").await?;
        self.socket
            .write_all(
                heapless::format!(U32_MAX_STR_LEN; "{}", sid)
                    .unwrap()
                    .as_bytes(),
            )
            .await?;
        self.socket.write_all(&DELIM).await?;
        Ok(())
    }
    async fn unsubscribe(&mut self, sid: usize) -> Result<()> {
        self.socket.write_all(b"UNSUB ").await?;
        self.socket
            .write_all(
                heapless::format!(U32_MAX_STR_LEN; "{}", sid)
                    .unwrap()
                    .as_bytes(),
            )
            .await?;
        self.socket.write_all(&DELIM).await?;
        Ok(())
    }
    async fn publish(&mut self, topic: C::Topic, data: C::MsgBuf) -> Result<()> {
        let data = data.as_bytes();

        // Header
        self.socket.write_all(b"PUB ").await?;
        self.socket.write_all(topic.as_str().as_bytes()).await?;
        self.socket.write_all(b" ").await?;
        self.socket
            .write_all(
                heapless::format!(U32_MAX_STR_LEN; "{}", data.len())
                    .unwrap()
                    .as_bytes(),
            )
            .await?;
        self.socket.write_all(&DELIM).await?;

        // Body
        self.socket.write_all(data).await?;
        self.socket.write_all(&DELIM).await?;

        Ok(())
    }
    async fn register_sub(&mut self, topic: C::Topic, channel: MsgSender<'a, C>) -> Result<()> {
        if self
            .subs
            .iter_mut()
            .find(|(_, tp, _, _)| tp == &topic)
            .is_some()
        {
            defmt!(error!("can't subscribe to a topic twice"));
            return Ok(());
        }

        let sid = self.subs.len();

        self.subs
            .push((sid, topic.clone(), channel, true))
            .map_err(|_| CapacityError::Subscriptions)?;

        self.subscribe(sid, topic).await
    }
    async fn deactivate(&mut self, topic: C::Topic) -> Result<()> {
        let sid = if let Some((sid, _, _, active)) =
            self.subs.iter_mut().find(|(_, tp, _, _)| tp == &topic)
        {
            *active = false;
            *sid
        } else {
            defmt!(error!(
                "can't unsubscribe, as no subscription was registered"
            ));
            return Ok(());
        };

        self.unsubscribe(sid).await
    }
    async fn reactivate(&mut self, topic: C::Topic) -> Result<()> {
        let sid = if let Some((sid, _, _, active)) =
            self.subs.iter_mut().find(|(_, tp, _, _)| tp == &topic)
        {
            *active = true;
            *sid
        } else {
            defmt!(error!(
                "can't resubscribe, as no subscription was registered"
            ));
            return Ok(());
        };

        self.subscribe(sid, topic).await
    }
    async fn run_connected(&mut self) {
        if let Err(e) =
            match select(self.socket.wait_read_ready(), self.cmd_channel.receive()).await {
                Either::First(()) => self.read().await,
                Either::Second(cmd) => match cmd {
                    InternalCmd::Sub(topic, ch) => self.register_sub(topic, ch).await,
                    InternalCmd::Pub(topic, data) => self.publish(topic, data).await,
                    InternalCmd::Unsub(topic) => self.deactivate(topic).await,
                    InternalCmd::Resub(topic) => self.reactivate(topic).await,
                },
            }
        {
            defmt!(error!("nats error: {}", e));
            match e {
                // If we had a connection issue disconnect
                Error::Disconnected | Error::Tcp(_) => self.disconnect().await,
                // otherwise reset framer
                _ => self.framer = Framer::new(),
            }
        };
    }
    async fn run_auth_step(&mut self) {
        if let Err(_e) = self.read().await {
            defmt!(error!("nats error: {}", _e));
            self.disconnect().await;
        }
    }
    async fn try_connect(&mut self) {
        match self.socket.connect(self.address).await {
            Ok(()) => {
                self.framer = Framer::new();
                self.state = State::Authenticating;
            }
            Err(_e) => defmt!(error!(
                "could not connect to nats: {}",
                defmt::Debug2Format(&_e)
            )),
        }
    }
    /// Mainloop entry for the runner
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
impl<'a, C: NatsCollections, A: NatsAuthenticator, const N: usize> Drop for Runner<'a, C, A, N> {
    fn drop(&mut self) {
        self.socket.abort();
    }
}
