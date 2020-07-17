mod event;
mod message;
mod tests;

use std::{
    collections::HashSet,
    fmt::{self, Debug, Display, Formatter},
    hash::Hash,
    time::Duration,
};

use futures::FutureExt;
use rand::Rng;
use serde::{de::DeserializeOwned, Serialize};
use smallvec::smallvec;
use tracing::{debug, error};

use crate::{
    components::{small_network::NodeId, storage::Storage, Component},
    effect::{
        requests::{NetworkRequest, StorageRequest},
        EffectBuilder, EffectExt, Effects,
    },
    utils::{GossipAction, GossipTable},
    GossipTableConfig,
};

pub use event::Event;
pub use message::Message;

pub trait Item: Clone + Serialize + DeserializeOwned + Send + Sync + Debug + Display {
    type Id: Copy + Eq + Hash + Debug + Display + Serialize + DeserializeOwned + Send + Sync;

    fn id(&self) -> &Self::Id;
}

pub(crate) fn put_deploy_to_store<T, REv>(
    effect_builder: EffectBuilder<REv>,
    deploy: crate::types::Deploy,
    maybe_sender: Option<NodeId>,
) -> Effects<Event<crate::types::Deploy>>
where
    T: Item,
    REv: From<Event<T>>
        + From<NetworkRequest<NodeId, Message<T>>>
        + From<StorageRequest<Storage>>
        + Send,
{
    let deploy_hash = *deploy.id();
    effect_builder
        .put_deploy_to_storage(deploy)
        .event(move |result| Event::PutToHolderResult {
            item_id: deploy_hash,
            maybe_sender,
            result: result.map(|_| ()).map_err(|error| format!("{}", error)),
        })
}

pub(crate) fn get_deploy_from_store<T, REv>(
    effect_builder: EffectBuilder<REv>,
    deploy_hash: crate::types::DeployHash,
    sender: NodeId,
) -> Effects<Event<crate::types::Deploy>>
where
    T: Item,
    REv: From<Event<T>>
        + From<NetworkRequest<NodeId, Message<T>>>
        + From<StorageRequest<Storage>>
        + Send,
{
    effect_builder
        .get_deploys_from_storage(smallvec![deploy_hash])
        .event(move |mut result| {
            let result = if result.len() == 1 {
                result.pop().unwrap().map_err(|error| format!("{}", error))
            } else {
                Err(String::from("expected a single result"))
            };
            Event::GetFromHolderResult {
                item_id: deploy_hash,
                requester: sender,
                result: Box::new(result),
            }
        })
}

/// The component which gossips to peers and handles incoming gossip messages from peers.
#[allow(clippy::type_complexity)]
pub(crate) struct Gossiper<T, REv>
where
    T: Item,
    REv: From<Event<T>>
        + From<NetworkRequest<NodeId, Message<T>>>
        + From<StorageRequest<Storage>>
        + Send
        + 'static,
{
    table: GossipTable<T::Id>,
    gossip_timeout: Duration,
    get_from_peer_timeout: Duration,
    put_to_holder:
        Box<dyn Fn(EffectBuilder<REv>, T, Option<NodeId>) -> Effects<Event<T>> + Send + 'static>,
    get_from_holder:
        Box<dyn Fn(EffectBuilder<REv>, T::Id, NodeId) -> Effects<Event<T>> + Send + 'static>,
}

impl<T, REv> Gossiper<T, REv>
where
    T: Item + 'static,
    <T as Item>::Id: 'static,
    REv: From<Event<T>>
        + From<NetworkRequest<NodeId, Message<T>>>
        + From<StorageRequest<Storage>>
        + Send,
{
    /// Constructs a new gossiper component.
    ///
    /// `put_to_holder` is called by the gossiper whenever a new complete item is received, via
    /// handling either an `Event::ItemReceived` or a `Message::GetResponse`.
    ///
    /// For an example of how `put_to_holder` should be implemented, see
    /// `gossiper::put_deploy_to_store()` which is used by `Gossiper<Deploy>`.
    ///
    /// `get_from_holder` is called by the gossiper when handling either a `Message::GossipResponse`
    /// where the sender indicates it needs the full item, or a `Message::GetRequest`.
    ///
    /// For an example of how `get_from_holder` should be implemented, see
    /// `gossiper::get_deploy_from_store()` which is used by `Gossiper<Deploy>`.
    pub(crate) fn new(
        config: GossipTableConfig,
        put_to_holder: impl Fn(EffectBuilder<REv>, T, Option<NodeId>) -> Effects<Event<T>>
            + Send
            + 'static,
        get_from_holder: impl Fn(EffectBuilder<REv>, T::Id, NodeId) -> Effects<Event<T>>
            + Send
            + 'static,
    ) -> Self {
        Gossiper {
            table: GossipTable::new(config),
            gossip_timeout: Duration::from_secs(config.gossip_request_timeout_secs()),
            get_from_peer_timeout: Duration::from_secs(config.get_remainder_timeout_secs()),
            put_to_holder: Box::new(put_to_holder),
            get_from_holder: Box::new(get_from_holder),
        }
    }

    /// Handles a new item received from somewhere other than a peer (e.g. the HTTP API server).
    fn handle_item_received(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item: T,
    ) -> Effects<Event<T>> {
        // Put the item to the component responsible for holding it.
        (self.put_to_holder)(effect_builder, item, None)
    }

    /// Gossips the given item ID to `count` random peers excluding the indicated ones.
    fn gossip(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item_id: T::Id,
        count: usize,
        exclude_peers: HashSet<NodeId>,
    ) -> Effects<Event<T>> {
        let message = Message::Gossip(item_id);
        effect_builder
            .gossip_message(message, count, exclude_peers)
            .event(move |peers| Event::GossipedTo { item_id, peers })
    }

    /// Handles the response from the network component detailing which peers it gossiped to.
    fn gossiped_to(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item_id: T::Id,
        peers: HashSet<NodeId>,
    ) -> Effects<Event<T>> {
        // We don't have any peers to gossip to, so pause the process, which will eventually result
        // in the entry being removed.
        if peers.is_empty() {
            self.table.pause(&item_id);
            debug!(
                "paused gossiping {} since no more peers to gossip to",
                item_id
            );
            return Effects::new();
        }

        // Set timeouts to check later that the specified peers all responded.
        peers
            .into_iter()
            .map(|peer| {
                effect_builder
                    .set_timeout(self.gossip_timeout)
                    .map(move |_| smallvec![Event::CheckGossipTimeout { item_id, peer }])
                    .boxed()
            })
            .collect()
    }

    /// Checks that the given peer has responded to a previous gossip request we sent it.
    fn check_gossip_timeout(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item_id: T::Id,
        peer: NodeId,
    ) -> Effects<Event<T>> {
        match self.table.check_timeout(&item_id, peer) {
            GossipAction::ShouldGossip(should_gossip) => self.gossip(
                effect_builder,
                item_id,
                should_gossip.count,
                should_gossip.exclude_peers,
            ),
            GossipAction::Noop => Effects::new(),
            GossipAction::GetRemainder { .. } | GossipAction::AwaitingRemainder => {
                unreachable!("can't have gossiped if we don't hold the complete data")
            }
        }
    }

    /// Checks that the given peer has responded to a previous gossip response or `GetRequest` we
    /// sent it indicating we wanted to get the full item from it.
    fn check_get_from_peer_timeout(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item_id: T::Id,
        peer: NodeId,
    ) -> Effects<Event<T>> {
        match self.table.remove_holder_if_unresponsive(&item_id, peer) {
            GossipAction::ShouldGossip(should_gossip) => self.gossip(
                effect_builder,
                item_id,
                should_gossip.count,
                should_gossip.exclude_peers,
            ),

            GossipAction::GetRemainder { holder } => {
                // The previous peer failed to provide the item, so we still need to get it.  Send
                // a `GetRequest` to a different holder and set a timeout to check we got the
                // response.
                let request = Message::GetRequest(item_id);
                let mut effects = effect_builder.send_message(holder, request).ignore();
                effects.extend(
                    effect_builder
                        .set_timeout(self.get_from_peer_timeout)
                        .event(move |_| Event::CheckGetFromPeerTimeout {
                            item_id,
                            peer: holder,
                        }),
                );
                effects
            }

            GossipAction::Noop | GossipAction::AwaitingRemainder => Effects::new(),
        }
    }

    /// Handles an incoming gossip request from a peer on the network.
    fn handle_gossip(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item_id: T::Id,
        sender: NodeId,
    ) -> Effects<Event<T>> {
        match self.table.new_partial_data(&item_id, sender) {
            GossipAction::ShouldGossip(should_gossip) => {
                // Gossip the item ID and send a response to the sender indicating we already hold
                // the item.
                let mut effects = self.gossip(
                    effect_builder,
                    item_id,
                    should_gossip.count,
                    should_gossip.exclude_peers,
                );
                let reply = Message::GossipResponse {
                    item_id,
                    is_already_held: true,
                };
                effects.extend(effect_builder.send_message(sender, reply).ignore());
                effects
            }
            GossipAction::GetRemainder { .. } => {
                // Send a response to the sender indicating we want the full item from them, and set
                // a timeout for this response.
                let reply = Message::GossipResponse {
                    item_id,
                    is_already_held: false,
                };
                let mut effects = effect_builder.send_message(sender, reply).ignore();
                effects.extend(
                    effect_builder
                        .set_timeout(self.get_from_peer_timeout)
                        .event(move |_| Event::CheckGetFromPeerTimeout {
                            item_id,
                            peer: sender,
                        }),
                );
                effects
            }
            GossipAction::Noop | GossipAction::AwaitingRemainder => {
                // Send a response to the sender indicating we already hold the item.
                let reply = Message::GossipResponse {
                    item_id,
                    is_already_held: true,
                };
                effect_builder.send_message(sender, reply).ignore()
            }
        }
    }

    /// Handles an incoming gossip response from a peer on the network.
    fn handle_gossip_response(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item_id: T::Id,
        is_already_held: bool,
        sender: NodeId,
    ) -> Effects<Event<T>> {
        let mut effects: Effects<_> = Effects::new();
        let action = if is_already_held {
            self.table.already_infected(&item_id, sender)
        } else {
            // `sender` doesn't hold the full item; treat this as a `GetRequest`.
            effects.extend(self.handle_get_request(effect_builder, item_id, sender));
            self.table.we_infected(&item_id, sender)
        };

        match action {
            GossipAction::ShouldGossip(should_gossip) => effects.extend(self.gossip(
                effect_builder,
                item_id,
                should_gossip.count,
                should_gossip.exclude_peers,
            )),
            GossipAction::Noop => (),
            GossipAction::GetRemainder { .. } | GossipAction::AwaitingRemainder => {
                unreachable!("can't have gossiped if we don't hold the complete item")
            }
        }

        effects
    }

    /// Handles an incoming `GetRequest` from a peer on the network.
    fn handle_get_request(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item_id: T::Id,
        sender: NodeId,
    ) -> Effects<Event<T>> {
        // Get the item from the component responsible for holding it, then send it to `sender`.
        (self.get_from_holder)(effect_builder, item_id, sender)
    }

    /// Handles an incoming `GetResponse` from a peer on the network.
    fn handle_get_response(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item: T,
        sender: NodeId,
    ) -> Effects<Event<T>> {
        // Put the item to the component responsible for holding it.
        (self.put_to_holder)(effect_builder, item, Some(sender))
    }

    /// Handles the `Ok` case for a `Result` of attempting to put the item to the component
    /// responsible for holding it, having received it from the sender (for the `Some` case) or from
    /// our own HTTP API server (the `None` case).
    fn handle_put_to_holder_success(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item_id: T::Id,
        maybe_sender: Option<NodeId>,
    ) -> Effects<Event<T>> {
        if let Some(should_gossip) = self.table.new_complete_data(&item_id, maybe_sender) {
            self.gossip(
                effect_builder,
                item_id,
                should_gossip.count,
                should_gossip.exclude_peers,
            )
        } else {
            Effects::new()
        }
    }

    /// Handles the `Err` case for a `Result` of attempting to put the item to the component
    /// responsible for holding it.
    fn failed_to_put_to_holder(&mut self, item_id: T::Id, error: String) -> Effects<Event<T>> {
        self.table.pause(&item_id);
        error!(
            "paused gossiping {} since failed to put to holder component: {}",
            item_id, error
        );
        Effects::new()
    }

    /// Handles the `Ok` case for a `Result` of attempting to get the item from the component
    /// responsible for holding it, in order to send it to the requester.
    fn got_from_holder(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        item: T,
        requester: NodeId,
    ) -> Effects<Event<T>> {
        let message = Message::GetResponse(Box::new(item));
        effect_builder.send_message(requester, message).ignore()
    }

    /// Handles the `Err` case for a `Result` of attempting to get the item from the component
    /// responsible for holding it.
    fn failed_to_get_from_holder(&mut self, item_id: T::Id, error: String) -> Effects<Event<T>> {
        self.table.pause(&item_id);
        error!(
            "paused gossiping {} since failed to get from store: {}",
            item_id, error
        );
        Effects::new()
    }
}

impl<T, REv> Component<REv> for Gossiper<T, REv>
where
    T: Item + 'static,
    REv: From<Event<T>>
        + From<NetworkRequest<NodeId, Message<T>>>
        + From<StorageRequest<Storage>>
        + Send,
{
    type Event = Event<T>;

    fn handle_event<R: Rng + ?Sized>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        _rng: &mut R,
        event: Self::Event,
    ) -> Effects<Self::Event> {
        debug!(?event, "handling event");
        match event {
            Event::ItemReceived { item } => self.handle_item_received(effect_builder, *item),
            Event::GossipedTo { item_id, peers } => {
                self.gossiped_to(effect_builder, item_id, peers)
            }
            Event::CheckGossipTimeout { item_id, peer } => {
                self.check_gossip_timeout(effect_builder, item_id, peer)
            }
            Event::CheckGetFromPeerTimeout { item_id, peer } => {
                self.check_get_from_peer_timeout(effect_builder, item_id, peer)
            }
            Event::MessageReceived { message, sender } => match message {
                Message::Gossip(item_id) => self.handle_gossip(effect_builder, item_id, sender),
                Message::GossipResponse {
                    item_id,
                    is_already_held,
                } => self.handle_gossip_response(effect_builder, item_id, is_already_held, sender),
                Message::GetRequest(item_id) => {
                    self.handle_get_request(effect_builder, item_id, sender)
                }
                Message::GetResponse(item) => {
                    self.handle_get_response(effect_builder, *item, sender)
                }
            },
            Event::PutToHolderResult {
                item_id,
                maybe_sender,
                result,
            } => match result {
                Ok(()) => self.handle_put_to_holder_success(effect_builder, item_id, maybe_sender),
                Err(error) => self.failed_to_put_to_holder(item_id, error),
            },
            Event::GetFromHolderResult {
                item_id,
                requester,
                result,
            } => match *result {
                Ok(item) => self.got_from_holder(effect_builder, item, requester),
                Err(error) => self.failed_to_get_from_holder(item_id, error),
            },
        }
    }
}

impl<T, REv> Debug for Gossiper<T, REv>
where
    T: Item,
    REv: From<Event<T>>
        + From<NetworkRequest<NodeId, Message<T>>>
        + From<StorageRequest<Storage>>
        + Send,
{
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        formatter
            .debug_struct("Gossiper")
            .field("table", &self.table)
            .field("gossip_timeout", &self.gossip_timeout)
            .field("get_from_peer_timeout", &self.get_from_peer_timeout)
            .finish()
    }
}
