use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Threshold in seconds before a peer is considered stale.
const STALE_THRESHOLD_SECS: u64 = 600;

/// Threshold in seconds before a peer is pruned entirely.
const PRUNE_THRESHOLD_SECS: u64 = 1800;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct PeerAnnounce {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub node_id: String,
    pub version: String,
    pub timestamp: u64,
    #[serde(default)]
    pub sender: Option<String>,
    #[serde(default)]
    pub node_list: Option<Vec<String>>,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub friendly_name: Option<String>,
    #[serde(default)]
    pub cpu_percent: Option<f32>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    #[serde(default)]
    pub thread_count: Option<u32>,
    #[serde(default)]
    pub neighbor_count: Option<u32>,
    #[serde(default)]
    pub uptime_secs: Option<u64>,
    #[serde(default)]
    pub msgs_received: Option<u64>,
    #[serde(default)]
    pub msgs_sent: Option<u64>,
    #[serde(default)]
    pub last_msg_age_secs: Option<u64>,
    #[serde(default)]
    pub reconnect_count: Option<u64>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub build_type: Option<String>,
    #[serde(default)]
    pub neighbors: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PeerState {
    pub node_id: String,
    pub friendly_name: Option<String>,
    pub version: String,
    pub last_seen: u64,
    pub stale: bool,
    pub cpu_percent: Option<f32>,
    pub memory_mb: Option<u64>,
    pub thread_count: Option<u32>,
    pub neighbor_count: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub msgs_received: Option<u64>,
    pub msgs_sent: Option<u64>,
    pub last_msg_age_secs: Option<u64>,
    pub reconnect_count: Option<u64>,
    pub os: Option<String>,
    pub build_type: Option<String>,
    pub neighbors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopologyEdge {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SwarmSnapshot {
    pub peers: Vec<PeerState>,
    pub edges: Vec<TopologyEdge>,
    pub total_peers: usize,
    pub active_peers: usize,
    pub stale_peers: usize,
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct PeerRegistry {
    peers: RwLock<HashMap<String, PeerState>>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self {
            peers: RwLock::new(HashMap::new()),
        }
    }

    /// Update (or insert) a peer from an incoming announce message.
    pub fn update(&self, announce: &PeerAnnounce) {
        if announce.msg_type != "peer_announce" {
            return;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let state = PeerState {
            node_id: announce.node_id.clone(),
            friendly_name: announce.friendly_name.clone(),
            version: announce.version.clone(),
            last_seen: now,
            stale: false,
            cpu_percent: announce.cpu_percent,
            memory_mb: announce.memory_mb,
            thread_count: announce.thread_count,
            neighbor_count: announce.neighbor_count,
            uptime_secs: announce.uptime_secs,
            msgs_received: announce.msgs_received,
            msgs_sent: announce.msgs_sent,
            last_msg_age_secs: announce.last_msg_age_secs,
            reconnect_count: announce.reconnect_count,
            os: announce.os.clone(),
            build_type: announce.build_type.clone(),
            neighbors: announce.neighbors.clone().unwrap_or_default(),
        };

        let mut map = self.peers.write().unwrap();
        map.insert(announce.node_id.clone(), state);
    }

    /// Build a point-in-time snapshot of the swarm.
    pub fn snapshot(&self) -> SwarmSnapshot {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut map = self.peers.write().unwrap();

        // Mark stale peers
        for peer in map.values_mut() {
            peer.stale = now.saturating_sub(peer.last_seen) > STALE_THRESHOLD_SECS;
        }

        let mut peers: Vec<PeerState> = map.values().cloned().collect();
        peers.sort_by(|a, b| {
            let name_a = a.friendly_name.as_deref().unwrap_or(&a.node_id);
            let name_b = b.friendly_name.as_deref().unwrap_or(&b.node_id);
            name_a.to_lowercase().cmp(&name_b.to_lowercase())
        });

        // Build deduplicated topology edges
        let mut edge_set: HashSet<(String, String)> = HashSet::new();
        for peer in &peers {
            for neighbor in &peer.neighbors {
                let pair = if peer.node_id < *neighbor {
                    (peer.node_id.clone(), neighbor.clone())
                } else {
                    (neighbor.clone(), peer.node_id.clone())
                };
                edge_set.insert(pair);
            }
        }
        let mut edges: Vec<TopologyEdge> = edge_set
            .into_iter()
            .map(|(source, target)| TopologyEdge { source, target })
            .collect();
        edges.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));

        let total_peers = peers.len();
        let stale_peers = peers.iter().filter(|p| p.stale).count();
        let active_peers = total_peers - stale_peers;

        SwarmSnapshot {
            peers,
            edges,
            total_peers,
            active_peers,
            stale_peers,
            timestamp: now,
        }
    }

    /// Remove peers not seen within the prune threshold. Returns count removed.
    pub fn prune_stale(&self) -> usize {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut map = self.peers.write().unwrap();
        let before = map.len();
        map.retain(|_, peer| now.saturating_sub(peer.last_seen) <= PRUNE_THRESHOLD_SECS);
        before - map.len()
    }
}
