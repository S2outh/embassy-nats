
use core::pin::pin;

use alloc::{string::String, vec::Vec};
use embassy_futures::select::select_slice;
use embassy_sync::channel::ReceiveFuture;

use crate::{CmdSender, InfoReceiver, InternalCmd, MsgChannel, MsgReceiver, NatsInfoMsg, NatsMsg, Storage};

pub struct Client<'a> {
    storage: &'a Storage<'a>,
    info_watch: InfoReceiver<'a>,
    cmd_channel: CmdSender<'a>,

    sub_vec: Vec<MsgReceiver<'a>>,
}
impl<'a> Client<'a> {
    pub(crate) fn new(
        storage: &'a Storage<'a>
    ) -> Self {
        let info_watch = storage.info_watch.dyn_anon_receiver();
        let cmd_channel = storage.cmd_channel.sender();
        Self {
            storage,
            info_watch,
            cmd_channel,
            sub_vec: Vec::new(),
        }
    }

    pub async fn publish(&mut self, topic: String, bytes: Vec<u8>) {
        self.cmd_channel.send(InternalCmd::Pub(topic, bytes)).await;
    }
    pub async fn subscribe(&mut self, topic: String, channel: &'a mut MsgChannel) {
        self.sub_vec.push(channel.receiver());
        self.cmd_channel.send(InternalCmd::Sub(topic, channel.sender())).await;
    }
    pub async fn receive(&mut self) -> NatsMsg {
        select_slice(pin!(&mut self.sub_vec.iter().map(|sub| sub.receive()).collect::<Vec<ReceiveFuture<'_, _, _, _>>>()[..])).await.0
    }
    pub async fn get_info(&mut self) -> Option<NatsInfoMsg> {
        self.info_watch.try_get()
    }
}
impl<'a> Clone for Client<'a> {
    fn clone(&self) -> Self {
        Client::new(self.storage)
    }
}
