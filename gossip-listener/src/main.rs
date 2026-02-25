use std::env;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use iroh::protocol::Router;
use iroh::SecretKey;
use iroh_gossip::api::Event;
use iroh_gossip::net::Gossip;
use serde::{Deserialize, Serialize};
use distributed_topic_tracker::{AutoDiscoveryGossip, RecordPublisher, TopicId as DttTopicId};

const TOPIC_STRING: &str = "gossipping/v1/all";
const DEFAULT_NODE_KEY_FILE: &str = "gossip_listener_node.key";
const DEFAULT_KNOWN_PEERS_FILE: &str = "gossip_listener_known_peers.txt";
const DEFAULT_DHT_SECRET: &str = "podping_gossip_default_secret";

// ---------------------------------------------------------------------------
// PeerAnnounce: periodic node ID announcement over gossip
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PeerAnnounce {
    #[serde(rename = "type")]
    msg_type: String,
    node_id: String,
    version: String,
    timestamp: u64,
}

impl PeerAnnounce {
    fn new(node_id: &str, version: &str) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self {
            msg_type: "peer_announce".to_string(),
            node_id: node_id.to_string(),
            version: version.to_string(),
            timestamp,
        }
    }
}

// ---------------------------------------------------------------------------
// Notification types (self-contained copy matching gossip-writer)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct GossipNotification {
    version: String,
    sender: String,
    timestamp: u64,
    medium: String,
    reason: String,
    iris: Vec<String>,
    signature: Option<String>,
}

/// Canonical form with fields in alphabetical order for signature verification.
#[derive(Debug, Serialize)]
struct CanonicalNotification<'a> {
    iris: &'a Vec<String>,
    medium: &'a str,
    reason: &'a str,
    sender: &'a str,
    timestamp: u64,
    version: &'a str,
}

impl GossipNotification {
    fn canonical_bytes(&self) -> Vec<u8> {
        let canonical = CanonicalNotification {
            iris: &self.iris,
            medium: &self.medium,
            reason: &self.reason,
            sender: &self.sender,
            timestamp: self.timestamp,
            version: &self.version,
        };
        serde_json::to_vec(&canonical).expect("canonical JSON serialization should not fail")
    }

    fn verify_signature(&self) -> Result<bool, String> {
        let sig_hex = match &self.signature {
            Some(s) => s,
            None => return Ok(false),
        };

        let pubkey_bytes: [u8; 32] = hex::decode(&self.sender)
            .map_err(|e| format!("bad sender hex: {e}"))?
            .try_into()
            .map_err(|_| "sender is not 32 bytes".to_string())?;

        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|e| format!("bad signature hex: {e}"))?
            .try_into()
            .map_err(|_| "signature is not 64 bytes".to_string())?;

        let verifying_key =
            VerifyingKey::from_bytes(&pubkey_bytes).map_err(|e| format!("bad pubkey: {e}"))?;
        let signature = Signature::from_bytes(&sig_bytes);

        let canonical = self.canonical_bytes();
        match verifying_key.verify(&canonical, &signature) {
            Ok(()) => Ok(true),
            Err(e) => Err(format!("signature verification failed: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== Gossip Listener ===\n");

    // --- Config from env vars ---
    let bootstrap_peer_ids_str = env::var("BOOTSTRAP_PEER_IDS").unwrap_or_default();
    let node_key_file =
        env::var("IROH_NODE_KEY_FILE").unwrap_or_else(|_| DEFAULT_NODE_KEY_FILE.to_string());
    let peers_file =
        env::var("KNOWN_PEERS_FILE").unwrap_or_else(|_| DEFAULT_KNOWN_PEERS_FILE.to_string());
    let dht_initial_secret = env::var("DHT_INITIAL_SECRET")
        .unwrap_or_else(|_| DEFAULT_DHT_SECRET.to_string());

    println!("  Topic: \"{}\"", TOPIC_STRING);
    println!("  DHT discovery: enabled");

    // --- Set up Iroh endpoint and gossip ---
    let node_key = load_or_create_node_key(&node_key_file)?;
    let node_key_bytes = node_key.to_bytes();
    let endpoint = iroh::Endpoint::builder()
        .secret_key(node_key)
        .bind()
        .await?;

    let my_node_id = endpoint.id();
    println!("  Iroh Node ID: {}", my_node_id);

    let gossip = Gossip::builder()
        .max_message_size(65536)
        .spawn(endpoint.clone());

    let _router = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();

    // --- DHT auto-discovery: subscribe to topic ---
    let dht_signing_key = ed25519_dalek::SigningKey::from_bytes(&node_key_bytes);
    let dtt_topic_id = DttTopicId::new(TOPIC_STRING.to_string());
    println!("  Topic ID: {}", hex::encode(dtt_topic_id.hash()));

    let record_publisher = RecordPublisher::new(
        dtt_topic_id,
        dht_signing_key.verifying_key(),
        dht_signing_key,
        None,
        dht_initial_secret.into_bytes(),
    );

    let topic = gossip
        .subscribe_and_join_with_auto_discovery_no_wait(record_publisher)
        .await?;
    let (gossip_sender, gossip_receiver) = topic.split().await?;
    println!("  Joined gossip topic with DHT auto-discovery.");

    // --- Optionally join bootstrap peers ---
    let mut bootstrap_peers: Vec<iroh::EndpointId> = bootstrap_peer_ids_str
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let file_peers = load_known_peers(&peers_file);
    for p in file_peers {
        if !bootstrap_peers.contains(&p) {
            bootstrap_peers.push(p);
        }
    }

    if bootstrap_peers.is_empty() {
        println!("  No additional bootstrap peers configured.");
    } else {
        println!("  Joining {} bootstrap peers...", bootstrap_peers.len());
        if let Err(e) = gossip_sender.join_peers(bootstrap_peers, None).await {
            eprintln!("  Warning: failed to join bootstrap peers: {}", e);
        }
    }

    // --- Periodic PeerAnnounce task ---
    let peer_announce_interval: u64 = env::var("PEER_ANNOUNCE_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    println!("  Announce interval: {}s", peer_announce_interval);

    if peer_announce_interval > 0 {
        let announce_sender = gossip_sender.clone();
        let announce_node_id = my_node_id.to_string();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(peer_announce_interval)).await;
                let announce = PeerAnnounce::new(&announce_node_id, env!("CARGO_PKG_VERSION"));
                match serde_json::to_vec(&announce) {
                    Ok(payload) => {
                        if let Err(e) = announce_sender.broadcast(payload).await {
                            eprintln!("[error] Failed to broadcast PeerAnnounce: {}", e);
                        } else {
                            println!("[info] Broadcast PeerAnnounce for {}", announce_node_id);
                        }
                    }
                    Err(e) => eprintln!("[error] Failed to serialize PeerAnnounce: {}", e),
                }
            }
        });
    }

    println!("\n  Listening for gossip notifications...\n");

    // --- Receive loop with Ctrl+C handling ---
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            item = gossip_receiver.next() => {
                match item {
                    Some(Ok(event)) => handle_event(event, &peers_file, &my_node_id),
                    Some(Err(e)) => eprintln!("[error] gossip stream error: {e}"),
                    None => {
                        println!("[info] gossip stream ended");
                        break;
                    }
                }
            }
            _ = &mut shutdown => {
                println!("\n[info] shutting down...");
                break;
            }
        }
    }

    endpoint.close().await;
    println!("[info] goodbye");
    Ok(())
}

fn handle_event(event: Event, peers_file: &str, my_node_id: &iroh::EndpointId) {
    match event {
        Event::Received(msg) => {
            let raw = &msg.content[..];
            // Try PeerAnnounce first
            if let Ok(announce) = serde_json::from_slice::<PeerAnnounce>(raw) {
                if announce.msg_type == "peer_announce" {
                    println!("[info] PeerAnnounce from {} v{}", announce.node_id, announce.version);
                    if let Ok(node_id) = announce.node_id.parse() {
                        save_peer_if_new(peers_file, &node_id, my_node_id);
                    }
                }
            } else {
                // Fall back to GossipNotification
                match serde_json::from_slice::<GossipNotification>(raw) {
                    Ok(notif) => print_notification(&notif),
                    Err(e) => {
                        eprintln!(
                            "[warn] failed to parse notification: {e}\n  raw: {}",
                            String::from_utf8_lossy(raw)
                        );
                    }
                }
            }
        }
        Event::NeighborUp(node_id) => {
            println!("[event] NeighborUp: {node_id}");
            save_peer_if_new(peers_file, &node_id, my_node_id);
        }
        Event::NeighborDown(node_id) => {
            println!("[event] NeighborDown: {node_id}");
        }
        Event::Lagged => {
            eprintln!("[warn] lagged — missed some messages");
        }
    }
}

fn print_notification(notif: &GossipNotification) {
    let sig_status = match notif.verify_signature() {
        Ok(true) => "VALID".to_string(),
        Ok(false) => "UNSIGNED".to_string(),
        Err(e) => format!("INVALID ({e})"),
    };

    println!("--- Notification ---");
    println!("  version:   {}", notif.version);
    println!("  sender:    {}", notif.sender);
    println!("  timestamp: {}", notif.timestamp);
    println!("  medium:    {}", notif.medium);
    println!("  reason:    {}", notif.reason);
    println!("  iris:      {:?}", notif.iris);
    println!("  signature: {}", sig_status);
    println!();
}

/// Load known peers from a text file (one NodeId per line).
/// Returns an empty vec if the file doesn't exist or can't be read.
fn load_known_peers(path: &str) -> Vec<iroh::EndpointId> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Save a peer's NodeId to the known-peers file if it's not already present
/// and not our own node ID. Caps the file at MAX_KNOWN_PEERS entries,
/// evicting the oldest (first) entries when full.
const MAX_KNOWN_PEERS: usize = 15;

fn save_peer_if_new(path: &str, node_id: &iroh::EndpointId, my_node_id: &iroh::EndpointId) {
    if node_id == my_node_id {
        return;
    }
    let node_str = node_id.to_string();
    let mut peers: Vec<String> = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if peers.iter().any(|l| l == &node_str) {
        return;
    }

    peers.push(node_str.clone());

    // Evict oldest entries if over the cap
    if peers.len() > MAX_KNOWN_PEERS {
        let drain_count = peers.len() - MAX_KNOWN_PEERS;
        peers.drain(..drain_count);
    }

    use std::io::Write;
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    if let Ok(mut f) = fs::File::create(path) {
        for p in &peers {
            let _ = writeln!(f, "{}", p);
        }
        println!("[info] Saved new peer to {}: {}", path, node_str);
    }
}

/// Load a persistent iroh node key from `path`, or generate a new one and save it.
fn load_or_create_node_key(path: &str) -> anyhow::Result<SecretKey> {
    if Path::new(path).exists() {
        let raw = fs::read(path)?;
        // Try string-based parse first (backward compat with iroh 0.32 format)
        if let Ok(s) = std::str::from_utf8(&raw) {
            if let Ok(key) = s.trim().parse::<SecretKey>() {
                println!("  Loaded iroh node key from {}", path);
                return Ok(key);
            }
        }
        // Fall back to raw 32-byte format
        let key_bytes: [u8; 32] = raw.try_into()
            .map_err(|_| anyhow::anyhow!("key file {} has invalid length", path))?;
        println!("  Loaded iroh node key from {}", path);
        Ok(SecretKey::from_bytes(&key_bytes))
    } else {
        let key = SecretKey::generate(&mut rand::rng());
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(path, key.to_bytes())?;
        println!("  Generated new iroh node key -> {}", path);
        Ok(key)
    }
}
