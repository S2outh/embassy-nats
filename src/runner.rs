use core::net::SocketAddr;

use embassy_futures::select::{Either, select};
use embassy_net::tcp::{self, TcpSocket};
use embedded_io_async::Write;
use thiserror::Error;

mod framer;
use framer::Frame;
use framer::Framer;

use crate::{
    AUTH_MAX_STR_LEN, BytesBuf, CapacityError, CmdReceiver, DELIM, InfoSender, InternalCmd,
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
enum Error<R> {
    #[error("Write error: {0}")]
    Write(#[from] tcp::Error),
    #[error("Framer error: {0}")]
    Framer(#[from] framer::FramerError<R>),
    #[error("Capacity of collection {0} not sufficient for operation")]
    Capacity(#[from] CapacityError),
    #[error("Json serialization error {0}")]
    Ser(#[from] serde_json_core::ser::Error),
}

pub struct Runner<'a, C: NatsCollections, A: NatsAuthenticator, const N: usize> {
    auth: A,
    state: State,
    address: SocketAddr,
    socket: TcpSocket<'a>,

    info_watch: InfoSender<'a>,
    cmd_channel: CmdReceiver<'a, C>,

    subs: heapless::Vec<(usize, C::Topic, MsgSender<'a, C>), N>,
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
    async fn read(&mut self) -> Result<(), Error<tcp::Error>> {
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
                let connect_msg_slice = connect_msg.extend_by(AUTH_MAX_STR_LEN)?;
                let len = serde_json_core::to_slice(&self.auth, connect_msg_slice)?;
                connect_msg.truncate(len);

                self.socket.write_all(b"CONNECT ").await?;
                self.socket.write_all(connect_msg.as_bytes()).await?;
                self.socket.write_all(&DELIM).await?;

                // resubscribe to all existing subscriptions
                let resubs: heapless::Vec<_, N> = self
                    .subs
                    .iter()
                    .map(|(sid, topic, _)| (*sid, topic.clone()))
                    .collect();
                for (sid, topic) in resubs {
                    self.subscribe(sid, topic).await?;
                }

                // Update state
                self.state = State::Connected;
            }
            Frame::Err => {
                self.disconnect().await;
            }
            Frame::Ok => (),
            Frame::Msg(nats_msg) => {
                if let Some((_, _, ch)) = self.subs.iter().find(|(sid, _, _)| sid == &nats_msg.sid)
                {
                    ch.send(nats_msg).await;
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
    async fn subscribe(&mut self, sid: usize, topic: C::Topic) -> Result<(), Error<tcp::Error>> {
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
    async fn unsubscribe(&mut self, sid: usize) -> Result<(), Error<tcp::Error>> {
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
    async fn publish(&mut self, topic: C::Topic, data: C::MsgBuf) -> Result<(), Error<tcp::Error>> {
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
    async fn register_sub(
        &mut self,
        topic: C::Topic,
        channel: MsgSender<'a, C>,
    ) -> Result<(), Error<tcp::Error>> {
        let sid = self.subs.len();

        self.subs
            .push((sid, topic.clone(), channel))
            .map_err(|_| CapacityError::Subscriptions)?;

        self.subscribe(sid, topic).await
    }
    async fn run_connected(&mut self) {
        if let Err(e) =
            match select(self.socket.wait_read_ready(), self.cmd_channel.receive()).await {
                Either::First(()) => self.read().await,
                Either::Second(cmd) => match cmd {
                    InternalCmd::Sub(topic, ch) => self.register_sub(topic, ch).await,
                    InternalCmd::Pub(topic, data) => self.publish(topic, data).await,
                },
            }
        {
            defmt!(error!("nats error: {}", e));
            match e {
                // If framer had a connection issue disconnect
                Error::Framer(framer::FramerError::Disconnected) |
                Error::Framer(framer::FramerError::Read(_))
                    => self.disconnect().await,
                // else reset framer
                Error::Framer(_) => self.framer = Framer::new(),
                // otherwise also disconnect
                _ => self.disconnect().await,
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

