//! Actor-based wrapper for iroh-gossip broadcast operations.

use actor_helper::{Action, Actor, Handle, Receiver, act};
use anyhow::Result;
use iroh::EndpointId;
use rand::seq::SliceRandom;

/// Gossip sender that broadcasts messages to peers.
///
/// Provides methods for broadcasting to all peers or just direct neighbors,
/// with peer joining capabilities for topology management.
#[derive(Debug, Clone)]
pub struct GossipSender {
    api: Handle<GossipSenderActor, anyhow::Error>,
    /// Direct handle to the iroh gossip sender, bypassing the actor queue.
    /// Used for join_peers which doesn't need to be serialized with broadcasts.
    direct_sender: iroh_gossip::api::GossipSender,
    _gossip: iroh_gossip::net::Gossip,
}

#[derive(Debug)]
pub struct GossipSenderActor {
    rx: Receiver<Action<GossipSenderActor>>,
    gossip_sender: iroh_gossip::api::GossipSender,
    _gossip: iroh_gossip::net::Gossip,
}

impl GossipSender {
    /// Create a new gossip sender from an iroh topic sender.
    pub fn new(
        gossip_sender: iroh_gossip::api::GossipSender,
        gossip: iroh_gossip::net::Gossip,
    ) -> Self {
        let direct_sender = gossip_sender.clone();
        let (api, rx) = Handle::channel();
        tokio::spawn({
            let gossip = gossip.clone();
            async move {
                let mut actor = GossipSenderActor {
                    rx,
                    gossip_sender,
                    _gossip: gossip.clone(),
                };
                let _ = actor.run().await;
            }
        });

        Self {
            api,
            direct_sender,
            _gossip: gossip,
        }
    }

    /// Broadcast a message to all peers in the topic.
    pub async fn broadcast(&self, data: Vec<u8>) -> Result<()> {
        tracing::debug!("GossipSender: broadcasting message ({} bytes)", data.len());
        self.api
            .call(act!(actor => async move {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    actor.gossip_sender.broadcast(data.into()),
                ).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(anyhow::anyhow!(e)),
                    Err(_) => {
                        tracing::warn!("GossipSender: broadcast timed out inside actor");
                        Err(anyhow::anyhow!("broadcast timed out"))
                    }
                }
            }))
            .await
    }

    /// Broadcast a message only to direct neighbors.
    pub async fn broadcast_neighbors(&self, data: Vec<u8>) -> Result<()> {
        tracing::debug!(
            "GossipSender: broadcasting to neighbors ({} bytes)",
            data.len()
        );
        self.api
            .call(act!(actor => async move {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    actor.gossip_sender.broadcast_neighbors(data.into()),
                ).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(anyhow::anyhow!(e)),
                    Err(_) => {
                        tracing::warn!("GossipSender: broadcast_neighbors timed out inside actor");
                        Err(anyhow::anyhow!("broadcast_neighbors timed out"))
                    }
                }
            }))
            .await
    }

    /// Join specific peer nodes, bypassing the actor queue.
    ///
    /// This sends the join command directly to the iroh gossip actor without
    /// going through the DttGossipSender actor. Use this for application-level
    /// peer joining (re-join, watchdog) to avoid blocking broadcasts.
    pub async fn join_peers_direct(&self, mut peers: Vec<EndpointId>, max_peers: Option<usize>) -> Result<()> {
        if let Some(max_peers) = max_peers {
            peers.shuffle(&mut rand::rng());
            peers.truncate(max_peers);
        }

        tracing::debug!("GossipSender: joining {} peers (direct)", peers.len());

        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.direct_sender.join_peers(peers),
        ).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
            Err(_) => {
                tracing::warn!("GossipSender: join_peers_direct timed out");
                Err(anyhow::anyhow!("join_peers timed out"))
            }
        }
    }

    /// Join specific peer nodes via the actor queue.
    ///
    /// # Arguments
    ///
    /// * `peers` - List of node IDs to join
    /// * `max_peers` - Optional maximum number of peers to join (randomly selected if exceeded)
    pub async fn join_peers(&self, peers: Vec<EndpointId>, max_peers: Option<usize>) -> Result<()> {
        let mut peers = peers;
        if let Some(max_peers) = max_peers {
            peers.shuffle(&mut rand::rng());
            peers.truncate(max_peers);
        }

        tracing::debug!("GossipSender: joining {} peers", peers.len());

        self.api
            .call(act!(actor => async move {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    actor.gossip_sender.join_peers(peers),
                ).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(anyhow::anyhow!(e)),
                    Err(_) => {
                        tracing::warn!("GossipSender: join_peers timed out inside actor");
                        Err(anyhow::anyhow!("join_peers timed out"))
                    }
                }
            }))
            .await
    }
}

impl Actor<anyhow::Error> for GossipSenderActor {
    async fn run(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                    action(self).await;
                }
                else => break Ok(()),
            }
        }
    }
}
