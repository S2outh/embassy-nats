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

#[derive(serde::Deserialize, Clone)]
pub struct NatsInfoMsg {
    server_id: String,
    server_name: String,
    version: String,
    go: String,
    host: String,
    port: i32,
    headers: bool,
    max_payload: i32,
    proto: i32,
}

type MsgChannel<'a> = channel::DynamicSender<'a, Vec<u8>>;

type InfoWatch = watch::Watch<ThreadModeRawMutex, NatsInfoMsg, 0>;
type CmdChannel<'a> = channel::Channel<ThreadModeRawMutex, InternalCmd<'a>, 1>;

enum InternalCmd<'a> {
    Pub(String, Vec<u8>),
    Sub(String, MsgChannel<'a>)
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

pub fn new_with_user_pwd<'d, 'a>(
    user: &str,
    pwd: &str,
    address: SocketAddr,
    socket: TcpSocket<'d>,
    storage: &'a mut Storage<'a>,
) -> (Client<'a>, Runner<'d, 'a, UserPwdAuthenticator>) {
    let auth = UserPwdAuthenticator::new(user, pwd);

    let client = Client::new(storage.info_watch.dyn_anon_receiver(), storage.cmd_channel.sender());
    let runner = Runner::new(auth, address, socket, storage.info_watch.sender(), storage.cmd_channel.receiver());

    (client, runner)
}

