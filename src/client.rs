
use alloc::{string::String, vec::Vec};
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, channel, watch};

use crate::{InternalCmd, NatsInfoMsg};

type InfoWatch<'a> = watch::DynAnonReceiver<'a, NatsInfoMsg>;
type CmdChannel<'a> = channel::Sender<'a, ThreadModeRawMutex, InternalCmd<'a>, 1>;

const RECV_BUF: usize = 5;

pub struct Client<'a> {
    info_watch: InfoWatch<'a>,
    cmd_channel: CmdChannel<'a>,

    sub_vec: Vec<channel::Channel<ThreadModeRawMutex, Vec<u8>, RECV_BUF>>,
}
impl<'a> Client<'a> {
    pub(crate) fn new(
        info_watch: InfoWatch<'a>,
        cmd_channel: CmdChannel<'a>,
    ) -> Self {
        Self {
            info_watch,
            cmd_channel,
            sub_vec: Vec::new(),
        }
    }

    pub async fn publish(&mut self, topic: String, bytes: Vec<u8>) {
        self.cmd_channel.send(InternalCmd::Pub(topic, bytes)).await;
    }
    pub async fn subscribe(&'a mut self, topic: String) {
        let channel = self.sub_vec.push_mut(channel::Channel::new());
        self.cmd_channel.send(InternalCmd::Sub(topic, channel.dyn_sender())).await;
    }
    pub async fn get_info(&mut self) -> Option<NatsInfoMsg> {
        self.info_watch.try_get()
    }
}
