#![no_std]

mod runner;
mod client;

use core::net::SocketAddr;

use heapless::CapacityError;
use embassy_net::tcp::TcpSocket;
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, channel, watch};
pub use runner::Runner;
pub use client::Client;

pub trait NatsConfig {
    type Topic: StrBuf;
    type Msg: BytesBuf;
    type Buf: BytesBuf;
}

pub trait StrBuf: Sized + Clone {
    fn try_from_str(s: &str) -> Result<Self, CapacityError>;
    fn as_str(&self) -> &str;
}

pub trait BytesBuf: Sized + Default {
    fn try_from_bytes(b: &[u8]) -> Result<Self, CapacityError>;
    fn len(&self) -> usize;
    fn push(&mut self, b: u8) -> Result<(), CapacityError>;
    fn as_bytes(&self) -> &[u8];
    fn clear(&mut self);
}

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
pub struct Alloc;

#[cfg(feature = "alloc")]
impl NatsConfig for Alloc {
    type Topic = alloc::string::String;
    type Msg = alloc::vec::Vec<u8>;
    type Buf = alloc::vec::Vec<u8>;
}

#[cfg(feature = "alloc")]
impl StrBuf for alloc::string::String {
    fn try_from_str(s: &str) -> Result<Self, CapacityError> {
        Ok(alloc::string::String::from(s))
    }
    fn as_str(&self) -> &str {
        <&str>::from(self)
    }
}

#[cfg(feature = "alloc")]
impl BytesBuf for alloc::vec::Vec<u8> {
    fn try_from_bytes(b: &[u8]) -> Result<Self, CapacityError> {
        Ok(alloc::vec::Vec::from(b))
    }
    fn len(&self) -> usize {
        self.as_bytes().len()
    }
    fn push(&mut self, b: u8) -> Result<(), CapacityError> {
        Ok(self.push(b))
    }
    fn as_bytes(&self) -> &[u8] {
        &self
    }
    fn clear(&mut self) {
        self.clear();
    }
}

pub struct Heapless<const TOPIC: usize, const PAYLOAD: usize, const BUF: usize>;

impl<const TOPIC: usize, const PAYLOAD: usize, const BUF: usize> NatsConfig for Heapless<TOPIC, PAYLOAD, BUF> {
    type Topic = heapless::String<TOPIC>;
    type Msg = heapless::Vec<u8, PAYLOAD>;
    type Buf = heapless::Vec<u8, BUF>;
}

impl<const TOPIC: usize> StrBuf for heapless::String<TOPIC> {
    fn try_from_str(s: &str) -> Result<Self, CapacityError> {
        heapless::String::try_from(s)
    }
    fn as_str(&self) -> &str {
        <&str>::from(self)
    }
}

impl<const PAYLOAD: usize> BytesBuf for heapless::Vec<u8, PAYLOAD> {
    fn try_from_bytes(b: &[u8]) -> Result<Self, CapacityError> {
        heapless::Vec::try_from(b)
    }
    fn len(&self) -> usize {
        self.as_bytes().len()
    }
    fn push(&mut self, b: u8) -> Result<(), CapacityError> {
        self.push(b).map_err(|_| CapacityError::default())
    }
    fn as_bytes(&self) -> &[u8] {
        &self
    }
    fn clear(&mut self) {
        self.clear();
    }
}


pub struct NatsMsg<C>
where C: NatsConfig {
    pub sid: usize,
    pub topic: C::Topic,
    pub data: C::Msg,
}

// server_id and server_name are up to 56 chars.
// 64 is therefore a good upper bound for this metadata.
type NatsMetadataString = heapless::String<64>;
#[derive(serde::Deserialize, Clone)]
pub struct NatsInfoMsg {
    pub server_id: NatsMetadataString,
    pub server_name: NatsMetadataString,
    pub version: NatsMetadataString,
    pub go: NatsMetadataString,
    pub host: NatsMetadataString,
    pub port: i32,
    pub headers: bool,
    pub max_payload: i32,
    pub proto: i32,
}


pub type MsgChannel<C, const N: usize> = channel::Channel<ThreadModeRawMutex, NatsMsg<C>, N>;
type MsgSender<'a, C> = channel::DynamicSender<'a, NatsMsg<C>>;
type MsgReceiver<'a, C> = channel::DynamicReceiver<'a, NatsMsg<C>>;

type InfoWatch = watch::Watch<ThreadModeRawMutex, NatsInfoMsg, 0>;
type InfoSender<'a> = watch::Sender<'a, ThreadModeRawMutex, NatsInfoMsg, 0>;
type InfoReceiver<'a> = watch::DynAnonReceiver<'a, NatsInfoMsg>;

type CmdChannel<'a, C> = channel::Channel<ThreadModeRawMutex, InternalCmd<'a, C>, 1>;
type CmdSender<'a, C> = channel::Sender<'a, ThreadModeRawMutex, InternalCmd<'a, C>, 1>;
type CmdReceiver<'a, C> = channel::Receiver<'a, ThreadModeRawMutex, InternalCmd<'a, C>, 1>;

enum InternalCmd<'a, C>
where C: NatsConfig {
    Pub(C::Topic, C::Msg),
    Sub(C::Topic, MsgSender<'a, C>)
}

pub trait NatsAuthenticator: serde::Serialize {}

type UsrPassString = heapless::String<20>;
#[derive(serde::Serialize)]
pub struct UserPwdAuthenticator {
    verbose: bool,
    pedantic: bool,
    tls_required: bool,
    user: UsrPassString,
    pass: UsrPassString,
    lang: &'static str,
    name: &'static str,
    version: &'static str,
}
impl UserPwdAuthenticator {
    fn new(user: &str, pwd: &str) -> Result<Self, CapacityError> {
        Ok(Self {
            verbose: false,
            pedantic: false,
            tls_required: false,
            user: user.try_into()?,
            pass: pwd.try_into()?,
            name: env!("CARGO_PKG_NAME"),
            lang: "rust",
            version: env!("CARGO_PKG_VERSION"),
        })
    }
}
impl NatsAuthenticator for UserPwdAuthenticator {}

type TokenString = heapless::String<32>;
#[derive(serde::Serialize)]
pub struct TokenAuthenticator {
    verbose: bool,
    pedantic: bool,
    tls_required: bool,
    token: TokenString,
    lang: &'static str,
    name: &'static str,
    version: &'static str,
}
impl TokenAuthenticator {
    fn new(auth_token: &str) -> Result<Self, CapacityError> {
        Ok(Self {
            verbose: false,
            pedantic: false,
            tls_required: false,
            token: auth_token.try_into()?,
            name: env!("CARGO_PKG_NAME"),
            lang: "rust",
            version: env!("CARGO_PKG_VERSION"),
        })
    }
}
impl NatsAuthenticator for TokenAuthenticator {}

pub struct Storage<'a, C>
where C: NatsConfig {
    info_watch: InfoWatch,
    cmd_channel: CmdChannel<'a, C>,
}
impl<'a, C> Storage<'a, C>
where C: NatsConfig {
    pub const fn new() -> Self {
        let info_watch = InfoWatch::new();
        let cmd_channel = CmdChannel::new();
        
        Self { info_watch, cmd_channel }
    }
}

pub fn new_with_user_pwd<'a, C, const N: usize>(
    user: &str,
    pwd: &str,
    address: SocketAddr,
    socket: TcpSocket<'a>,
    storage: &'a Storage<'a, C>,
) -> Result<(Client<'a, C, N>, Runner<'a, C, UserPwdAuthenticator, N>), CapacityError>
where C: NatsConfig {
    let auth = UserPwdAuthenticator::new(user, pwd)?;

    let runner = Runner::new(auth, address, socket, storage.info_watch.sender(), storage.cmd_channel.receiver());
    let client = Client::new(storage);

    Ok((client, runner))
}

pub fn new_with_auth_token<'a, C, const N: usize>(
    auth_token: &str,
    address: SocketAddr,
    socket: TcpSocket<'a>,
    storage: &'a Storage<'a, C>,
) -> Result<(Client<'a, C, N>, Runner<'a, C, TokenAuthenticator, N>), CapacityError>
where C: NatsConfig {
    let auth = TokenAuthenticator::new(auth_token)?;

    let runner = Runner::new(auth, address, socket, storage.info_watch.sender(), storage.cmd_channel.receiver());
    let client = Client::new(storage);

    Ok((client, runner))
}


