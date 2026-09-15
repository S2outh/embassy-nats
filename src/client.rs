use core::{pin::Pin, sync::atomic::Ordering};

use embassy_futures::select::select_slice;
use embassy_sync::channel::DynamicReceiveFuture;

use crate::{
    CapacityError, CmdSender, InfoReceiver, InternalCmd, MsgChannel, MsgReceiver, NatsCollections,
    NatsInfoMsg, NatsMsg, Storage,
};

/// The client struct can be used to interface with the runner.
/// it can be cloned to receive new interfaces to the same runner.
pub struct Client<'a, C, const N: usize>
where
    C: NatsCollections,
{
    storage: &'a Storage<'a, C>,
    info_watch: InfoReceiver<'a>,
    cmd_channel: CmdSender<'a, C>,

    sub_vec: heapless::Vec<MsgReceiver<'a, C>, N>,
}
impl<'a, C, const N: usize> Client<'a, C, N>
where
    C: NatsCollections,
{
    pub(crate) fn new(storage: &'a Storage<'a, C>) -> Self {
        let info_watch = storage.info_watch.dyn_anon_receiver();
        let cmd_channel = storage.cmd_channel.sender();
        Self {
            storage,
            info_watch,
            cmd_channel,
            sub_vec: heapless::Vec::new(),
        }
    }

    /// Publish a message with a given topic
    pub async fn publish(&mut self, topic: C::Topic, bytes: C::MsgBuf) {
        self.cmd_channel.send(InternalCmd::Pub(topic, bytes)).await;
    }

    /// Subscribe to a given topic. Since the lifetime of the message channel needs to outlive
    /// the entire NATS stack (which in practice almost always means 'static) the user will need to
    /// provide one. Note that currently there is no method to unsubscribe.
    ///
    /// Returns a capacity error if the number of subscriptions the runner can handle are exausted
    pub async fn subscribe<const S: usize>(
        &mut self,
        topic: C::Topic,
        channel: &'a MsgChannel<C, S>,
    ) -> Result<(), CapacityError> {
        if self.storage.sub_count.load(Ordering::Acquire) >= N {
            return Err(CapacityError::Subscriptions);
        }
        self.storage.sub_count.fetch_add(1, Ordering::Release);
        self.sub_vec
            .push(channel.receiver().into())
            .map_err(|_| CapacityError::Subscriptions)?;
        self.cmd_channel
            .send(InternalCmd::Sub(topic, channel.sender().into()))
            .await;
        Ok(())
    }
    /// Awaiting receive will await any message from all subscriptions of this client.
    /// If there are no subscriptions this will hang forever
    pub async fn receive(&mut self) -> NatsMsg<C> {
        let mut futs = self
            .sub_vec
            .iter()
            .map(|sub| sub.receive())
            .collect::<heapless::Vec<DynamicReceiveFuture<'_, _>, N>>();
        select_slice(Pin::new(&mut futs[..])).await.0
    }

    /// Get the contents of the latest INFO message received from the server
    pub async fn get_info(&mut self) -> Option<NatsInfoMsg> {
        self.info_watch.try_get()
    }
}
impl<'a, C, const N: usize> Clone for Client<'a, C, N>
where
    C: NatsCollections,
{
    fn clone(&self) -> Self {
        Client::new(self.storage)
    }
}
