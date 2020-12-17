// Copyright 2020 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::ProtocolConfig,
    helper,
    packet::Message as MessagePacket,
    peer::PeerManager,
    tangle::{MessageMetadata, MsTangle},
    worker::{
        message_submitter::MessageSubmitterError, BroadcasterWorker, BroadcasterWorkerEvent, MessageRequesterWorker,
        MetricsWorker, MilestoneValidatorWorker, MilestoneValidatorWorkerEvent, PeerManagerWorker, PropagatorWorker,
        PropagatorWorkerEvent, RequestedMessages, StorageWorker, TangleWorker,
    },
    ProtocolMetrics,
};

use bee_common::{packable::Packable, shutdown_stream::ShutdownStream};
use bee_common_pt2::{node::Node, worker::Worker};
use bee_message::{payload::Payload, Message, MessageId, MESSAGE_ID_LENGTH};
use bee_network::PeerId;

use async_trait::async_trait;
use blake2::{
    digest::{Update, VariableOutput},
    VarBlake2b,
};
use futures::{channel::oneshot::Sender, stream::StreamExt};
use log::{error, info, trace, warn};
use tokio::sync::mpsc;

use std::{any::TypeId, convert::Infallible};

pub(crate) struct ProcessorWorkerEvent {
    pub(crate) pow_score: f64,
    pub(crate) from: Option<PeerId>,
    pub(crate) message_packet: MessagePacket,
    pub(crate) notifier: Option<Sender<Result<MessageId, MessageSubmitterError>>>,
}

pub(crate) struct ProcessorWorker {
    pub(crate) tx: mpsc::UnboundedSender<ProcessorWorkerEvent>,
}

#[async_trait]
impl<N: Node> Worker<N> for ProcessorWorker {
    type Config = (ProtocolConfig, u64);
    type Error = Infallible;

    fn dependencies() -> &'static [TypeId] {
        vec![
            TypeId::of::<StorageWorker>(),
            TypeId::of::<TangleWorker>(),
            TypeId::of::<MilestoneValidatorWorker>(),
            TypeId::of::<PropagatorWorker>(),
            TypeId::of::<BroadcasterWorker>(),
            TypeId::of::<MessageRequesterWorker>(),
            TypeId::of::<MetricsWorker>(),
            TypeId::of::<PeerManagerWorker>(),
        ]
        .leak()
    }

    async fn start(node: &mut N, config: Self::Config) -> Result<Self, Self::Error> {
        let (tx, rx) = mpsc::unbounded_channel();
        let milestone_validator = node.worker::<MilestoneValidatorWorker>().unwrap().tx.clone();
        let propagator = node.worker::<PropagatorWorker>().unwrap().tx.clone();
        let broadcaster = node.worker::<BroadcasterWorker>().unwrap().tx.clone();
        let message_requester = node.worker::<MessageRequesterWorker>().unwrap().tx.clone();

        let _storage = node.storage();
        let tangle = node.resource::<MsTangle<N::Backend>>();
        let requested_messages = node.resource::<RequestedMessages>();
        let metrics = node.resource::<ProtocolMetrics>();
        let peer_manager = node.resource::<PeerManager>();

        node.spawn::<Self, _, _>(|shutdown| async move {
            info!("Running.");

            let mut receiver = ShutdownStream::new(shutdown, rx);
            let mut blake2b = VarBlake2b::new(MESSAGE_ID_LENGTH).unwrap();

            while let Some(ProcessorWorkerEvent {
                pow_score,
                from,
                message_packet,
                notifier,
            }) = receiver.next().await
            {
                trace!("Processing received message...");

                let message = match Message::unpack(&mut &message_packet.bytes[..]) {
                    Ok(message) => message,
                    Err(e) => {
                        trace!("Invalid message: {:?}.", e);
                        metrics.invalid_messages_inc();
                        if let Some(tx) = notifier {
                            notify_err(format!("Invalid message: {:?}.", e), tx).await;
                        }
                        continue;
                    }
                };

                if message.network_id() != config.1 {
                    trace!("Incompatible network ID {} != {}.", message.network_id(), config.1);
                    metrics.invalid_messages_inc();
                    if let Some(tx) = notifier {
                        notify_err(
                            format!("Incompatible network ID {} != {}.", message.network_id(), config.1),
                            tx,
                        )
                        .await;
                    }
                    continue;
                }

                // TODO should be passed by the hasher worker ?
                blake2b.update(&message_packet.bytes);
                let mut bytes = [0u8; 32];
                // TODO Do we have to copy ?
                blake2b.finalize_variable_reset(|digest| bytes.copy_from_slice(&digest));
                let message_id = MessageId::from(bytes);

                if pow_score < config.0.minimum_pow_score {
                    trace!(
                        "Insufficient pow score: {} < {}.",
                        pow_score,
                        config.0.minimum_pow_score
                    );
                    metrics.invalid_messages_inc();
                    if let Some(tx) = notifier {
                        notify_err(
                            format!(
                                "Insufficient pow score: {} < {}.",
                                pow_score, config.0.minimum_pow_score
                            ),
                            tx,
                        )
                        .await;
                    }
                    continue;
                }

                let requested = requested_messages.contains_key(&message_id);

                let mut metadata = MessageMetadata::arrived();
                metadata.flags_mut().set_requested(requested);

                // store message
                if let Some(message) = tangle.insert(message, message_id, metadata).await {
                    if let Some(tx) = notifier {
                        notify_message_id(message_id, tx).await;
                    }

                    // TODO this was temporarily moved from the tangle.
                    // Reason is that since the tangle is not a worker, it can't have access to the propagator tx.
                    // When the tangle is made a worker, this should be put back on.

                    if let Err(e) = propagator.send(PropagatorWorkerEvent(message_id)) {
                        error!("Failed to send message id {} to propagator: {:?}.", message_id, e);
                    }

                    metrics.new_messages_inc();

                    match requested_messages.remove(&message_id) {
                        Some((_, (index, _))) => {
                            // Message was requested.
                            let parent1 = message.parent1();
                            let parent2 = message.parent2();

                            helper::request_message(&tangle, &message_requester, &*requested_messages, *parent1, index)
                                .await;
                            if parent1 != parent2 {
                                helper::request_message(
                                    &tangle,
                                    &message_requester,
                                    &*requested_messages,
                                    *parent2,
                                    index,
                                )
                                .await;
                            }
                        }
                        None => {
                            // Message was not requested.
                            if let Err(e) = broadcaster.send(BroadcasterWorkerEvent {
                                source: from,
                                message: message_packet,
                            }) {
                                warn!("Broadcasting message failed: {}.", e);
                            }
                        }
                    };

                    match message.payload() {
                        Some(Payload::Milestone(_)) => {
                            if let Err(e) = milestone_validator.send(MilestoneValidatorWorkerEvent(message_id)) {
                                error!(
                                    "Sending message id {} to milestone validation failed: {:?}.",
                                    message_id, e
                                );
                            }
                        }
                        Some(Payload::Indexation(_payload)) => {
                            // TODO when protocol backend is merged
                            // let index = payload.hash();
                            // storage.insert(&index, &message_id);
                        }
                        _ => {}
                    }
                } else {
                    metrics.known_messages_inc();
                    if let Some(peer_id) = from {
                        if let Some(peer) = peer_manager.peers.get(&peer_id) {
                            peer.metrics.known_messages_inc();
                        }
                    }
                    if let Some(tx) = notifier {
                        notify_message_id(message_id, tx).await;
                    }
                }
            }

            info!("Stopped.");
        });

        Ok(Self { tx })
    }
}

async fn notify_err(err: String, notifier: Sender<Result<MessageId, MessageSubmitterError>>) {
    if let Err(e) = notifier.send(Err(MessageSubmitterError(err))) {
        error!("Failed to send error: {:?}.", e);
    }
}

async fn notify_message_id(message_id: MessageId, notifier: Sender<Result<MessageId, MessageSubmitterError>>) {
    if let Err(e) = notifier.send(Ok(message_id)) {
        error!("Failed to send message id: {:?}.", e);
    }
}
