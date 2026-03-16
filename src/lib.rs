#![no_std]
#![feature(never_type)]

mod runner;
mod client;

use core::net::SocketAddr;

use alloc::{string::String, vec::Vec};
use embassy_net::tcp::TcpSocket;
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, channel, watch};
pub use runner::Runner;
pub use client::Client;

extern crate alloc;

pub struct NatsMsg {
    pub sid: usize,
    pub topic: String,
    pub data: Vec<u8>,
}

#[derive(serde::Deserialize, Clone)]
pub struct NatsInfoMsg {
    pub server_id: String,
    pub server_name: String,
    pub version: String,
    pub go: String,
    pub host: String,
    pub port: i32,
    pub headers: bool,
    pub max_payload: i32,
    pub proto: i32,
}

const RECV_BUF: usize = 5;

type MsgChannel = channel::Channel<ThreadModeRawMutex, NatsMsg, RECV_BUF>;
type MsgSender<'a> = channel::Sender<'a, ThreadModeRawMutex, NatsMsg, RECV_BUF>;
type MsgReceiver<'a> = channel::Receiver<'a, ThreadModeRawMutex, NatsMsg, RECV_BUF>;

type InfoWatch = watch::Watch<ThreadModeRawMutex, NatsInfoMsg, 0>;
type InfoSender<'a> = watch::Sender<'a, ThreadModeRawMutex, NatsInfoMsg, 0>;
type InfoReceiver<'a> = watch::DynAnonReceiver<'a, NatsInfoMsg>;

const CMD_BUF: usize = 1;

type CmdChannel<'a> = channel::Channel<ThreadModeRawMutex, InternalCmd<'a>, CMD_BUF>;
type CmdSender<'a> = channel::Sender<'a, ThreadModeRawMutex, InternalCmd<'a>, CMD_BUF>;
type CmdReceiver<'a> = channel::Receiver<'a, ThreadModeRawMutex, InternalCmd<'a>, CMD_BUF>;

enum InternalCmd<'a> {
    Pub(String, Vec<u8>),
    Sub(String, MsgSender<'a>)
}

pub trait NatsAuthenticator {
    fn connect_msg(&self) -> String;
}

#[derive(serde::Serialize)]
pub struct UserPwdAuthenticator {
    verbose: bool,
    pedantic: bool,
    tls_required: bool,
    user: String,
    pass: String,
    lang: String,
    name: String,
    version: String,
}
impl UserPwdAuthenticator {
    fn new(user: &str, pwd: &str) -> Self {
        Self {
            verbose: false,
            pedantic: false,
            tls_required: false,
            user: String::from(user),
            pass: String::from(pwd),
            name: String::from(env!("CARGO_PKG_NAME")),
            lang: String::from("rust"),
            version: String::from(env!("CARGO_PKG_VERSION")),
        }
    }
}
impl NatsAuthenticator for UserPwdAuthenticator {
    fn connect_msg(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

pub struct Storage<'a> {
    info_watch: InfoWatch,
    cmd_channel: CmdChannel<'a>,
}
impl<'a> Storage<'a> {
    pub fn new() -> Self {
        let info_watch = InfoWatch::new();
        let cmd_channel = CmdChannel::new();
        
        Self { info_watch, cmd_channel }
    }
}

pub fn new_with_user_pwd<'a>(
    user: &str,
    pwd: &str,
    address: SocketAddr,
    socket: TcpSocket<'a>,
    storage: &'a mut Storage<'a>,
) -> (Client<'a>, Runner<'a, UserPwdAuthenticator>) {
    let auth = UserPwdAuthenticator::new(user, pwd);

    let client = Client::new(storage.info_watch.dyn_anon_receiver(), storage.cmd_channel.sender());
    let runner = Runner::new(auth, address, socket, storage.info_watch.sender(), storage.cmd_channel.receiver());

    (client, runner)
}

