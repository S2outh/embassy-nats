
use core::pin::Pin;

use embassy_futures::select::select_slice;
use embassy_sync::channel::DynamicReceiveFuture;
use heapless::{CapacityError, vec::Vec};

use crate::{CmdSender, InfoReceiver, InternalCmd, MsgChannel, MsgReceiver, NatsConfig, NatsInfoMsg, NatsMsg, Storage};

pub struct Client<'a, C, const N: usize>
where C: NatsConfig {
    storage: &'a Storage<'a, C>,
    info_watch: InfoReceiver<'a>,
    cmd_channel: CmdSender<'a, C>,

    sub_vec: Vec<MsgReceiver<'a, C>, N>,
}
impl<'a, C, const N: usize> Client<'a, C, N>
where C: NatsConfig {
    pub(crate) fn new(
        storage: &'a Storage<'a, C>
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

    /// Publish a message with a given topic
    pub async fn publish(&mut self, topic: C::Topic, bytes: C::Msg) {
        self.cmd_channel.send(InternalCmd::Pub(topic, bytes)).await;
    }

    /// Subscribe to a given topic. Since the lifetime of the message channel needs to outlive
    /// the entire NATS stack (which in practice almost always means 'static) the user will need to
    /// provide one.
    pub async fn subscribe<const S: usize>(&mut self, topic: C::Topic, channel: &'a MsgChannel<C, S>) -> Result<(), CapacityError> {
        self.sub_vec.push(channel.dyn_receiver()).map_err(|_| CapacityError::default())?;
        self.cmd_channel.send(InternalCmd::Sub(topic, channel.dyn_sender())).await;
        Ok(())
    }
    /// Awaiting receive will await any message from all subscriptions of this client.
    /// If there are no subscriptions this will hang forever
    pub async fn receive(&mut self) -> NatsMsg<C> {
        let mut futs = self.sub_vec.iter().map(|sub| sub.receive()).collect::<Vec<DynamicReceiveFuture<'_, _>, N>>();
        select_slice(Pin::new(&mut futs[..])).await.0
    }

    /// Get the contents of the latest INFO message received from the server
    pub async fn get_info(&mut self) -> Option<NatsInfoMsg> {
        self.info_watch.try_get()
    }
}
impl<'a, C, const N: usize> Clone for Client<'a, C, N>
where C: NatsConfig {
    fn clone(&self) -> Self {
        Client::new(self.storage)
    }
}
