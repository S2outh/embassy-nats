#![no_std]

mod client;
mod runner;

use core::net::SocketAddr;

pub use client::Client;
use embassy_net::tcp::TcpSocket;
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, channel, watch};
pub use runner::Runner;
use thiserror::Error;
use heapless::CapacityError as HeaplessErr;

// Private module to seal traits
mod sealed {
    pub trait Sealed {}
}

// constant implementation of usize::max
const fn max(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

// These constants set sane but addmitedly arbitrary upper bounds
// for the heapless string types in CONNECT and INFO messages

// Maximum length of user and password for auth
const USR_PASS_STR_SIZE: usize = 20;

// server_id and server_name are up to 56 chars.
// 64 is therefore a good upper bound for this metadata.
const INFO_METADATA_STR_SIZE: usize = 64;

// The maximum length of the (json) messages received by nats are important
// in order to allocate the correct heapless:: type lengths.
// Here is a collection of necessary constants and calculated sizes

// The NATS ending delimiter
const DELIM: [u8; 2] = *b"\r\n";

// for UserPasswordAuth (trivially the longest one with empty filelds),
// this equals to 112 bytes base (string with empty user / pwd / name / version)
// + max(user and password max length, token max length)
// + project name and version length:
const AUTH_MAX_STR_LEN: usize = 112
    + max(USR_PASS_STR_SIZE * 2, TOKEN_STR_SIZE)
    + env!("CARGO_PKG_NAME").len()
    + env!("CARGO_PKG_VERSION").len();

// The base (empty strings, i32::MIN) length for NatsInfoMsg equals to 142.
// The total length is prefix + base + 5 * max metadata string length + delimeter.
// This information is also used to compute the required size of the Header receive buffer
const INFO_MAX_STR_LEN: usize = "INFO ".len() + 145 + 5 * INFO_METADATA_STR_SIZE + DELIM.len();

// The size of U32 MAX in decimal is 10 bytes
const U32_MAX_STR_LEN: usize = 10;

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Error)]
pub enum CapacityError {
    #[error("C: StrBuf")]
    Str,
    #[error("C: BytesBuf")]
    Bytes,
    #[error("N: Subscriptions")]
    Subscriptions,
}

/// This trait is a container for collections used througout this library,
/// and can be used to switch between heapless and alloc types.
pub trait NatsCollections: sealed::Sealed {
    type Topic: StrBuf;
    type MsgBuf: BytesBuf;
    type SyncBuf: BytesBuf;
    type AuthBuf: BytesBuf;
}

/// A String buffer, containing methods to convert from and to &str
pub trait StrBuf: Sized + Clone {
    fn try_from_str(s: &str) -> Result<Self, CapacityError>;
    fn as_str(&self) -> &str;
}

/// A Bytes buffer, containing methods to extend, clear and retreive info from a buffer
pub trait BytesBuf: Sized + Default {
    /// This method tries to extend the buffer by len, and returns the mutable slice of data
    /// that has been added
    fn extend_by(&mut self, len: usize) -> Result<&mut [u8], CapacityError>;
    /// keep only the first len bytes, or less if the collection is not large enough
    fn truncate(&mut self, len: usize);
    fn clear(&mut self);
    fn as_bytes(&self) -> &[u8];
    fn len(&self) -> usize;
}

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
pub struct Alloc;

#[cfg(feature = "alloc")]
impl sealed::Sealed for Alloc {}

#[cfg(feature = "alloc")]
impl NatsCollections for Alloc {
    type Topic = alloc::string::String;
    type MsgBuf = alloc::vec::Vec<u8>;
    type SyncBuf = alloc::vec::Vec<u8>;
    type AuthBuf = alloc::vec::Vec<u8>;
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
    fn extend_by(&mut self, len: usize) -> Result<&mut [u8], CapacityError> {
        let before = self.len();
        self.extend(core::iter::repeat_n(0, len));
        Ok(&mut self[before..])
    }
    fn truncate(&mut self, len: usize) {
        self.truncate(len);
    }
    fn clear(&mut self) {
        self.clear();
    }
    fn as_bytes(&self) -> &[u8] {
        &self
    }
    fn len(&self) -> usize {
        self.as_bytes().len()
    }
}

pub struct Heapless<const TOPIC: usize, const PAYLOAD: usize>;

impl<const TOPIC: usize, const PAYLOAD: usize> sealed::Sealed for Heapless<TOPIC, PAYLOAD> {}

impl<const TOPIC: usize, const PAYLOAD: usize> NatsCollections for Heapless<TOPIC, PAYLOAD> {
    type Topic = heapless::String<TOPIC>;
    type MsgBuf = heapless::Vec<u8, PAYLOAD>;
    type SyncBuf = heapless::Vec<u8, INFO_MAX_STR_LEN>;
    type AuthBuf = heapless::Vec<u8, AUTH_MAX_STR_LEN>;
}

impl<const TOPIC: usize> StrBuf for heapless::String<TOPIC> {
    fn try_from_str(s: &str) -> Result<Self, CapacityError> {
        heapless::String::try_from(s).map_err(|_| CapacityError::Str)
    }
    fn as_str(&self) -> &str {
        <&str>::from(self)
    }
}

impl<const PAYLOAD: usize> BytesBuf for heapless::Vec<u8, PAYLOAD> {
    fn extend_by(&mut self, len: usize) -> Result<&mut [u8], CapacityError> {
        if self.capacity() - self.len() < len {
            return Err(CapacityError::Bytes)
        }
        let before = self.len();
        self.extend(core::iter::repeat_n(0, len));
        Ok(&mut self[before..])
    }
    fn truncate(&mut self, len: usize) {
        self.truncate(len);
    }
    fn clear(&mut self) {
        self.clear();
    }
    fn as_bytes(&self) -> &[u8] {
        &self
    }
    fn len(&self) -> usize {
        self.as_bytes().len()
    }
}

pub struct NatsMsg<C>
where
    C: NatsCollections,
{
    pub sid: usize,
    pub topic: C::Topic,
    pub data: C::MsgBuf,
}

#[derive(serde::Deserialize, Clone)]
pub struct NatsInfoMsg {
    pub server_id: heapless::String<INFO_METADATA_STR_SIZE>,
    pub server_name: heapless::String<INFO_METADATA_STR_SIZE>,
    pub version: heapless::String<INFO_METADATA_STR_SIZE>,
    pub go: heapless::String<INFO_METADATA_STR_SIZE>,
    pub host: heapless::String<INFO_METADATA_STR_SIZE>,
    pub port: i32,
    pub headers: bool,
    pub max_payload: i32,
    pub proto: i32,
}

pub type MsgChannel<C, const N: usize> = channel::Channel<ThreadModeRawMutex, NatsMsg<C>, N>;
type MsgSender<'a, C> = channel::SendDynamicSender<'a, NatsMsg<C>>;
type MsgReceiver<'a, C> = channel::SendDynamicReceiver<'a, NatsMsg<C>>;

type InfoWatch = watch::Watch<ThreadModeRawMutex, NatsInfoMsg, 0>;
type InfoSender<'a> = watch::Sender<'a, ThreadModeRawMutex, NatsInfoMsg, 0>;
type InfoReceiver<'a> = watch::DynAnonReceiver<'a, NatsInfoMsg>;

type CmdChannel<'a, C> = channel::Channel<ThreadModeRawMutex, InternalCmd<'a, C>, 1>;
type CmdSender<'a, C> = channel::Sender<'a, ThreadModeRawMutex, InternalCmd<'a, C>, 1>;
type CmdReceiver<'a, C> = channel::Receiver<'a, ThreadModeRawMutex, InternalCmd<'a, C>, 1>;

enum InternalCmd<'a, C>
where
    C: NatsCollections,
{
    Pub(C::Topic, C::MsgBuf),
    Sub(C::Topic, MsgSender<'a, C>),
}

pub trait NatsAuthenticator: serde::Serialize + sealed::Sealed {}

#[derive(serde::Serialize)]
pub struct NoopAuthenticator {
    verbose: bool,
    pedantic: bool,
    tls_required: bool,
    lang: &'static str,
    name: &'static str,
    version: &'static str,
}
impl NoopAuthenticator {
    fn new() -> Self {
        Self {
            verbose: false,
            pedantic: false,
            tls_required: false,
            name: env!("CARGO_PKG_NAME"),
            lang: "rust",
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

impl sealed::Sealed for NoopAuthenticator {}
impl NatsAuthenticator for NoopAuthenticator {}

#[derive(serde::Serialize)]
pub struct UserPwdAuthenticator {
    verbose: bool,
    pedantic: bool,
    tls_required: bool,
    user: heapless::String<USR_PASS_STR_SIZE>,
    pass: heapless::String<USR_PASS_STR_SIZE>,
    lang: &'static str,
    name: &'static str,
    version: &'static str,
}
impl UserPwdAuthenticator {
    fn new(user: &str, pwd: &str) -> Result<Self, HeaplessErr> {
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
impl sealed::Sealed for UserPwdAuthenticator {}
impl NatsAuthenticator for UserPwdAuthenticator {}

const TOKEN_STR_SIZE: usize = 32;
#[derive(serde::Serialize)]
pub struct TokenAuthenticator {
    verbose: bool,
    pedantic: bool,
    tls_required: bool,
    token: heapless::String<TOKEN_STR_SIZE>,
    lang: &'static str,
    name: &'static str,
    version: &'static str,
}
impl TokenAuthenticator {
    fn new(auth_token: &str) -> Result<Self, HeaplessErr> {
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
impl sealed::Sealed for TokenAuthenticator {}
impl NatsAuthenticator for TokenAuthenticator {}

pub struct Storage<'a, C>
where
    C: NatsCollections,
{
    info_watch: InfoWatch,
    cmd_channel: CmdChannel<'a, C>,
}
impl<'a, C> Storage<'a, C>
where
    C: NatsCollections,
{
    pub const fn new() -> Self {
        let info_watch = InfoWatch::new();
        let cmd_channel = CmdChannel::new();

        Self {
            info_watch,
            cmd_channel,
        }
    }
}

pub fn new_no_auth<'a, C, const N: usize>(
    address: SocketAddr,
    socket: TcpSocket<'a>,
    storage: &'a Storage<'a, C>,
) -> (Client<'a, C, N>, Runner<'a, C, NoopAuthenticator, N>)
where
    C: NatsCollections,
{
    let auth = NoopAuthenticator::new();

    let runner = Runner::new(
        auth,
        address,
        socket,
        storage.info_watch.sender(),
        storage.cmd_channel.receiver(),
    );
    let client = Client::new(storage);

    (client, runner)
}

pub fn new_with_user_pwd<'a, C, const N: usize>(
    user: &str,
    pwd: &str,
    address: SocketAddr,
    socket: TcpSocket<'a>,
    storage: &'a Storage<'a, C>,
) -> Result<(Client<'a, C, N>, Runner<'a, C, UserPwdAuthenticator, N>), HeaplessErr>
where
    C: NatsCollections,
{
    let auth = UserPwdAuthenticator::new(user, pwd)?;

    let runner = Runner::new(
        auth,
        address,
        socket,
        storage.info_watch.sender(),
        storage.cmd_channel.receiver(),
    );
    let client = Client::new(storage);

    Ok((client, runner))
}

pub fn new_with_auth_token<'a, C, const N: usize>(
    auth_token: &str,
    address: SocketAddr,
    socket: TcpSocket<'a>,
    storage: &'a Storage<'a, C>,
) -> Result<(Client<'a, C, N>, Runner<'a, C, TokenAuthenticator, N>), HeaplessErr>
where
    C: NatsCollections,
{
    let auth = TokenAuthenticator::new(auth_token)?;

    let runner = Runner::new(
        auth,
        address,
        socket,
        storage.info_watch.sender(),
        storage.cmd_channel.receiver(),
    );
    let client = Client::new(storage);

    Ok((client, runner))
}
