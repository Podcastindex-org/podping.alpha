mod archive;
mod notification;

use iroh::protocol::Router;
use iroh::SecretKey;
use iroh_gossip::net::Gossip;
use iroh_gossip::api::Event;
use distributed_topic_tracker::{GossipSender as DttGossipSender, GossipReceiver as DttGossipReceiver};
use std::collections::{HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::signal;
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use distributed_topic_tracker::{AutoDiscoveryGossip, RecordPublisher, TopicId as DttTopicId};

// Cap'n Proto plexo message wrapper
pub mod plexo_message_capnp {
    include!("../plexo-schemas/built/dev/plexo/plexo_message_capnp.rs");
}
use crate::plexo_message_capnp::plexo_message;

// Podping schema types for deserialization
use podping_schemas::org::podcastindex::podping::podping_write_capnp::podping_write;
use podping_schemas::org::podcastindex::podping::podping_medium_capnp::PodpingMedium;
use podping_schemas::org::podcastindex::podping::podping_reason_capnp::PodpingReason;
use podping_schemas::org::podcastindex::podping::hivewriter::podping_hive_transaction_capnp::podping_hive_transaction;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// PeerAnnounce: periodic node ID announcement over gossip.
// Also used for peer_endorse messages (trust propagation).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PeerAnnounce {
    #[serde(rename = "type")]
    msg_type: String,
    node_id: String,
    version: String,
    timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node_list: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    friendly_name: Option<String>,
}

// Canonical form for peer_endorse signing (alphabetical by serialized key name)
#[derive(Serialize)]
struct CanonicalPeerEndorse<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    friendly_name: Option<&'a str>,
    node_id: &'a str,
    node_list: &'a Vec<String>,
    sender: &'a str,
    timestamp: u64,
    #[serde(rename = "type")]
    msg_type: &'a str,
    version: &'a str,
}

impl PeerAnnounce {
    fn new(node_id: &str, version: &str, friendly_name: Option<String>) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self {
            msg_type: "peer_announce".to_string(),
            node_id: node_id.to_string(),
            version: version.to_string(),
            timestamp,
            sender: None,
            node_list: None,
            signature: None,
            friendly_name,
        }
    }

    fn new_endorse(node_id: &str, version: &str, sender: &str, endorsed_keys: Vec<String>, friendly_name: Option<String>) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self {
            msg_type: "peer_endorse".to_string(),
            node_id: node_id.to_string(),
            version: version.to_string(),
            timestamp,
            sender: Some(sender.to_string()),
            node_list: Some(endorsed_keys),
            signature: None,
            friendly_name,
        }
    }

    fn canonical_endorse_bytes(&self) -> Vec<u8> {
        let canonical = CanonicalPeerEndorse {
            friendly_name: self.friendly_name.as_deref(),
            node_id: &self.node_id,
            node_list: self.node_list.as_ref().expect("node_list required for endorse"),
            sender: self.sender.as_ref().expect("sender required for endorse"),
            timestamp: self.timestamp,
            msg_type: &self.msg_type,
            version: &self.version,
        };
        serde_json::to_vec(&canonical).expect("Canonical JSON serialization should not fail")
    }

    fn sign_endorse(&mut self, signing_key: &SigningKey) {
        let canonical = self.canonical_endorse_bytes();
        let sig = signing_key.sign(&canonical);
        self.signature = Some(hex::encode(sig.to_bytes()));
    }

    fn verify_endorse_signature(&self) -> Result<bool, String> {
        let sig_hex = match &self.signature {
            Some(s) => s,
            None => return Ok(false),
        };
        let sender_hex = match &self.sender {
            Some(s) => s,
            None => return Err("No sender field".to_string()),
        };

        let pubkey_bytes: [u8; 32] = hex::decode(sender_hex)
            .map_err(|e| format!("Bad sender hex: {e}"))?
            .try_into()
            .map_err(|_| "Sender is not 32 bytes".to_string())?;

        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|e| format!("Bad signature hex: {e}"))?
            .try_into()
            .map_err(|_| "Signature is not 64 bytes".to_string())?;

        let verifying_key =
            VerifyingKey::from_bytes(&pubkey_bytes).map_err(|e| format!("Bad pubkey: {e}"))?;
        let signature = Signature::from_bytes(&sig_bytes);

        let canonical = self.canonical_endorse_bytes();
        match verifying_key.verify(&canonical, &signature) {
            Ok(()) => Ok(true),
            Err(e) => Err(format!("Signature verification failed: {e}")),
        }
    }
}

// Defaults
const DEFAULT_ZMQ_BIND: &str = "tcp://0.0.0.0:9998";
const DEFAULT_KEY_FILE: &str = "/data/gossip/iroh.key";
const DEFAULT_NODE_KEY_FILE: &str = "/data/gossip/iroh_node.key";
const DEFAULT_ARCHIVE_PATH: &str = "/data/gossip/archive.db";
const DEFAULT_KNOWN_PEERS_FILE: &str = "/data/gossip/known_peers.txt";
const DEFAULT_TRUSTED_PUBLISHERS_FILE: &str = "/data/gossip/trusted_publishers.txt";
const TOPIC_STRING: &str = "gossipping/v1/all";
const DEFAULT_DHT_SECRET: &str = "podping_gossip_default_secret";
const DEFAULT_PEER_ENDORSE_INTERVAL: u64 = 600;
const REBOOTSTRAP_TIMEOUT: u64 = 180;
const BATCH_INTERVAL_SECS: u64 = 3;
const RECONNECT_AFTER_FAILURES: u64 = 5;
const BROADCAST_TIMEOUT_SECS: u64 = 10;
const MAX_RETRY_QUEUE: usize = 500;
const MAX_GOSSIP_PAYLOAD: usize = 60000; // Must stay under max_message_size (65536) with overhead

// A pending IRI extracted from a ZMQ message, waiting to be batched
struct PendingPing {
    iri: String,
    medium_str: &'static str,
    reason_str: &'static str,
    medium_raw: u16,
    reason_raw: u16,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Write tracing output to fd 3 if available, otherwise stderr
    let trace_writer: Box<dyn std::io::Write + Send + Sync> = unsafe {
        if libc::fcntl(3, libc::F_GETFD) != -1 {
            Box::new(std::fs::File::from_raw_fd(3))
        } else {
            Box::new(std::io::stderr())
        }
    };
    let trace_writer = std::sync::Mutex::new(trace_writer);
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(trace_writer)
        .init();

    // WORKAROUND: iroh-quinn 0.16.1 panic resilience
    // iroh-quinn has a bug where a connection panic triggers a double-panic abort
    // (first panic poisons a mutex, destructor panics acquiring it, Rust aborts).
    // This hook logs the iroh-quinn context clearly before the inevitable abort so
    // operators can distinguish it from application bugs. Docker restart: unless-stopped
    // handles the actual recovery. Remove this when iroh-quinn is updated past 0.16.1.
    std::panic::set_hook(Box::new(|info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown".to_string()
        };
        let location = info.location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let is_iroh = location.contains("iroh-quinn") || payload.contains("drained connections");
        let is_actor_shutdown = payload.contains("actor stopped")
            || payload.contains("must not be polled after");
        if is_iroh {
            eprintln!("\x1b[1;31m[PANIC] Known iroh-quinn 0.16.1 bug at {}: {}\x1b[0m", location, payload);
            eprintln!("\x1b[1;31m[PANIC] This is an upstream bug, not a gossip-writer issue. Process will restart via Docker.\x1b[0m");
        } else if is_actor_shutdown {
            // DTT's internal actors panic when the gossip system shuts down — benign during exit
            eprintln!("\x1b[33m[SHUTDOWN] DTT actor stopped (normal during shutdown)\x1b[0m");
        } else {
            eprintln!("\x1b[1;31m[PANIC] at {}: {}\x1b[0m", location, payload);
            let bt = std::backtrace::Backtrace::force_capture();
            eprintln!("{}", bt);
        }
    }));
    // END WORKAROUND

    // --- Configuration from environment ---
    let zmq_bind = env::var("ZMQ_BIND_ADDR").unwrap_or_else(|_| DEFAULT_ZMQ_BIND.to_string());
    let key_file = env::var("IROH_SECRET_FILE").unwrap_or_else(|_| DEFAULT_KEY_FILE.to_string());
    let node_key_file =
        env::var("IROH_NODE_KEY_FILE").unwrap_or_else(|_| DEFAULT_NODE_KEY_FILE.to_string());
    let archive_path =
        env::var("ARCHIVE_PATH").unwrap_or_else(|_| DEFAULT_ARCHIVE_PATH.to_string());
    let peers_file =
        env::var("KNOWN_PEERS_FILE").unwrap_or_else(|_| DEFAULT_KNOWN_PEERS_FILE.to_string());
    let bootstrap_peer_ids_str = env::var("BOOTSTRAP_PEER_IDS").unwrap_or_default();
    let peer_announce_interval: u64 = env::var("PEER_ANNOUNCE_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let dht_initial_secret = env::var("DHT_INITIAL_SECRET")
        .unwrap_or_else(|_| DEFAULT_DHT_SECRET.to_string());
    let trusted_publishers_file =
        env::var("TRUSTED_PUBLISHERS_FILE").unwrap_or_else(|_| DEFAULT_TRUSTED_PUBLISHERS_FILE.to_string());
    let peer_endorse_interval: u64 = env::var("PEER_ENDORSE_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PEER_ENDORSE_INTERVAL);
    let auto_trust_endorsements = env::var("AUTO_TRUST_ENDORSEMENTS")
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let archive_enabled = env::var("ARCHIVE_ENABLED")
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let friendly_name: Option<String> = env::var("NODE_FRIENDLY_NAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.len() > 64 {
                eprintln!("  Warning: NODE_FRIENDLY_NAME truncated to 64 characters");
                s.chars().take(64).collect::<String>()
            } else {
                s
            }
        });

    let trusted_publishers = Arc::new(RwLock::new(load_trusted_publishers(&trusted_publishers_file)));

    println!("gossip-writer v{}", env!("CARGO_PKG_VERSION"));
    println!("  ZMQ bind:     {}", zmq_bind);
    println!("  Key file:     {}", key_file);
    println!("  Node key:     {}", node_key_file);
    println!("  Archive:      {}", if archive_enabled { &archive_path } else { "disabled" });
    println!("  Peers file:   {}", peers_file);
    println!("  Topic:        {}", TOPIC_STRING);
    println!("  Announce interval: {}s", peer_announce_interval);
    println!("  Endorse interval: {}s", peer_endorse_interval);
    println!("  Auto-trust endorsements: {}", auto_trust_endorsements);
    if let Some(ref name) = friendly_name {
        println!("  Friendly name: {}", name);
    }
    println!("  DHT discovery: enabled");
    println!("  Trusted publishers file: {}", trusted_publishers_file);
    {
        let tp = trusted_publishers.read().unwrap();
        if tp.is_empty() {
            println!("  Trusted publisher filter: disabled (accepting all)");
        } else {
            println!("  Trusted publisher filter: {} senders", tp.len());
        }
    }

    // --- Load or generate ed25519 signing key ---
    let signing_key = notification::load_or_generate_key(&key_file)?;
    let pubkey_hex = notification::pubkey_hex(&signing_key);
    println!("  Sender pubkey: {}", pubkey_hex);

    // --- Optionally open SQLite archive ---
    let db = if archive_enabled {
        let archive = archive::Archive::open(&archive_path)?;
        println!("  Archive DB ready.");
        Some(archive)
    } else {
        None
    };

    // --- Set up Iroh endpoint and gossip ---
    // Load or create a persistent iroh node key (separate from the ed25519-dalek signing key)
    let node_key = load_or_create_node_key(&node_key_file)?;
    let node_key_bytes = node_key.to_bytes();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(node_key)
        .bind()
        .await?;

    let my_node_id = endpoint.id();
    println!("  Iroh Node ID: {}", my_node_id);

    let gossip = Gossip::builder()
        .max_message_size(65536)
        .spawn(endpoint.clone());

    // Register gossip protocol with the router for incoming connections
    let router = Router::builder(endpoint.clone())
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
        dht_initial_secret.clone().into_bytes(),
    );

    let topic = gossip
        .subscribe_and_join_with_auto_discovery_no_wait(record_publisher)
        .await?;
    let (gossip_sender, gossip_receiver) = topic.split().await?;
    println!("  Joined gossip topic with DHT auto-discovery.");

    // Shared sender so watchdog and broadcast task can both use (and reconnect can replace) it
    let shared_sender: Arc<tokio::sync::RwLock<DttGossipSender>> =
        Arc::new(tokio::sync::RwLock::new(gossip_sender));

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
        let sender = shared_sender.read().await;
        if let Err(e) = sender.join_peers(bootstrap_peers, None).await {
            eprintln!("  Warning: failed to join bootstrap peers: {}", e);
        }
    }

    // --- mpsc channel: ZMQ thread -> async broadcast task ---
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1000);
    let announce_tx = tx.clone();

    // --- Shutdown flag for the blocking ZMQ thread ---
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_zmq = shutdown.clone();

    // --- Re-bootstrap watchdog timer ---
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let last_notification_time = Arc::new(AtomicU64::new(now_secs));

    // Peer friendly name tracking
    let peer_names: Arc<RwLock<HashMap<String, String>>> = Arc::new(RwLock::new(HashMap::new()));

    // Health tracking counters
    let health_broadcasts_sent = Arc::new(AtomicU64::new(0));
    let health_broadcasts_failed = Arc::new(AtomicU64::new(0));
    let health_last_broadcast_ok = Arc::new(AtomicU64::new(now_secs));
    let health_broadcast_task_alive = Arc::new(AtomicBool::new(true));

    // Spawn the initial receive task
    spawn_receive_task(
        gossip_receiver,
        peers_file.clone(),
        my_node_id,
        trusted_publishers.clone(),
        trusted_publishers_file.clone(),
        auto_trust_endorsements,
        last_notification_time.clone(),
        peer_names.clone(),
    );

    // --- Async broadcast task (with reconnection) ---
    let broadcast_shared = shared_sender.clone();
    let broadcast_shutdown = shutdown.clone();
    let bcast_sent = health_broadcasts_sent.clone();
    let bcast_failed = health_broadcasts_failed.clone();
    let bcast_last_ok = health_last_broadcast_ok.clone();
    let bcast_alive = health_broadcast_task_alive.clone();
    let reconnect_endpoint = endpoint.clone();
    let reconnect_node_key_bytes = node_key_bytes;
    let reconnect_dht_secret = dht_initial_secret.clone();
    let reconnect_peers_file = peers_file.clone();
    let reconnect_my_node_id = my_node_id;
    let reconnect_trusted = trusted_publishers.clone();
    let reconnect_trusted_file = trusted_publishers_file.clone();
    let reconnect_auto_trust = auto_trust_endorsements;
    let reconnect_last_notif = last_notification_time.clone();
    let reconnect_peer_names = peer_names.clone();
    tokio::spawn(async move {
        // Keep the router alive in this task; on reconnect we replace it
        // (dropping the old router aborts its accept loop without closing the endpoint)
        let mut _current_router = router;
        let mut consecutive_failures: u64 = 0;
        let mut retry_queue: VecDeque<Vec<u8>> = VecDeque::new();
        let timeout_dur = std::time::Duration::from_secs(BROADCAST_TIMEOUT_SECS);

        while let Some(payload) = rx.recv().await {
            if broadcast_shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Try the new payload
            let result = {
                let sender = broadcast_shared.read().await;
                tokio::time::timeout(timeout_dur, sender.broadcast(payload.clone())).await
            };
            match result {
                Ok(Ok(_)) => {
                    bcast_sent.fetch_add(1, Ordering::Relaxed);
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                    bcast_last_ok.store(now, Ordering::Relaxed);
                    if consecutive_failures >= 3 {
                        println!("\x1b[32m[BROADCAST] Gossip broadcast recovered after {} failures\x1b[0m", consecutive_failures);
                    }
                    consecutive_failures = 0;
                }
                Ok(Err(e)) => {
                    bcast_failed.fetch_add(1, Ordering::Relaxed);
                    consecutive_failures += 1;
                    // Queue for retry
                    if retry_queue.len() < MAX_RETRY_QUEUE {
                        retry_queue.push_back(payload);
                    }
                    if consecutive_failures <= 3 {
                        eprintln!("\x1b[35m[WARN] Gossip broadcast error: {}\x1b[0m", e);
                    }
                }
                Err(_) => {
                    // Timeout — broadcast hung
                    bcast_failed.fetch_add(1, Ordering::Relaxed);
                    consecutive_failures += 1;
                    if retry_queue.len() < MAX_RETRY_QUEUE {
                        retry_queue.push_back(payload);
                    }
                    if consecutive_failures <= 3 {
                        eprintln!("\x1b[35m[WARN] Gossip broadcast timed out ({}s)\x1b[0m", BROADCAST_TIMEOUT_SECS);
                    }
                }
            }

            // Reconnect if enough consecutive failures
            if consecutive_failures == RECONNECT_AFTER_FAILURES {
                eprintln!("\x1b[1;31m[RECONNECT] {} consecutive broadcast failures — reconnecting gossip topic...\x1b[0m", consecutive_failures);
                let dht_key = ed25519_dalek::SigningKey::from_bytes(&reconnect_node_key_bytes);
                let dtt_topic = DttTopicId::new(TOPIC_STRING.to_string());
                let publisher = RecordPublisher::new(
                    dtt_topic,
                    dht_key.verifying_key(),
                    dht_key,
                    None,
                    reconnect_dht_secret.clone().into_bytes(),
                );
                // Spawn a fresh Gossip actor (the old one is dead after all topics quit)
                let new_gossip = Gossip::builder()
                    .max_message_size(65536)
                    .spawn(reconnect_endpoint.clone());
                // Re-register the new gossip with a fresh Router for incoming connections
                // Replace the router: dropping the old one stops its accept loop,
                // the new one routes incoming gossip connections to the fresh actor
                _current_router = Router::builder(reconnect_endpoint.clone())
                    .accept(iroh_gossip::ALPN, new_gossip.clone())
                    .spawn();
                match new_gossip
                    .subscribe_and_join_with_auto_discovery_no_wait(publisher)
                    .await
                {
                    Ok(new_topic) => match new_topic.split().await {
                        Ok((new_sender, new_receiver)) => {
                            {
                                let mut sender_guard = broadcast_shared.write().await;
                                *sender_guard = new_sender;
                            }

                            spawn_receive_task(
                                new_receiver,
                                reconnect_peers_file.clone(),
                                reconnect_my_node_id,
                                reconnect_trusted.clone(),
                                reconnect_trusted_file.clone(),
                                reconnect_auto_trust,
                                reconnect_last_notif.clone(),
                                reconnect_peer_names.clone(),
                            );

                            // Drain retry queue through new sender
                            let queued = retry_queue.len();
                            if queued > 0 {
                                println!("\x1b[32m[RECONNECT] Replaying {} queued broadcasts...\x1b[0m", queued);
                                let sender = broadcast_shared.read().await;
                                let mut replayed = 0;
                                while let Some(queued_payload) = retry_queue.pop_front() {
                                    match tokio::time::timeout(timeout_dur, sender.broadcast(queued_payload.clone())).await {
                                        Ok(Ok(_)) => {
                                            bcast_sent.fetch_add(1, Ordering::Relaxed);
                                            replayed += 1;
                                        }
                                        _ => {
                                            // New sender also failing — put it back and stop
                                            retry_queue.push_front(queued_payload);
                                            break;
                                        }
                                    }
                                }
                                println!("\x1b[32m[RECONNECT] Replayed {}/{} queued broadcasts\x1b[0m", replayed, queued);
                            }

                            consecutive_failures = 0;
                            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                            bcast_last_ok.store(now, Ordering::Relaxed);
                            println!("\x1b[32m[RECONNECT] Gossip topic reconnected successfully.\x1b[0m");
                        }
                        Err(e) => {
                            eprintln!("\x1b[1;31m[RECONNECT] Failed to split topic: {}. Will retry.\x1b[0m", e);
                            consecutive_failures = 0;
                        }
                    },
                    Err(e) => {
                        eprintln!("\x1b[1;31m[RECONNECT] Failed to re-subscribe: {}. Will retry on next batch.\x1b[0m", e);
                        consecutive_failures = 0;
                    }
                }
            }
        }
        bcast_alive.store(false, Ordering::Relaxed);
        eprintln!("\x1b[1;31m[HEALTH] Broadcast task exited!\x1b[0m");
    });

    // --- Periodic PeerAnnounce task ---
    if peer_announce_interval > 0 {
        let announce_node_id = my_node_id.to_string();
        let announce_friendly = friendly_name.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(peer_announce_interval)).await;
                let announce = PeerAnnounce::new(&announce_node_id, env!("CARGO_PKG_VERSION"), announce_friendly.clone());
                match serde_json::to_vec(&announce) {
                    Ok(payload) => {
                        if let Err(e) = announce_tx.send(payload).await {
                            eprintln!("\x1b[35m[WARN] Failed to queue PeerAnnounce: {}\x1b[0m", e);
                        } else {
                            println!("\x1b[33m[ANNOUNCE] Broadcast PeerAnnounce for {}\x1b[0m", announce_node_id);
                        }
                    }
                    Err(e) => eprintln!("\x1b[35m[WARN] Failed to serialize PeerAnnounce: {}\x1b[0m", e),
                }
            }
        });
    }

    // --- Periodic PeerEndorse broadcast task ---
    if peer_endorse_interval > 0 {
        let endorse_tx = tx.clone();
        let endorse_node_id = my_node_id.to_string();
        let endorse_pubkey = pubkey_hex.clone();
        let endorse_signing_key = signing_key.clone();
        let endorse_trusted = trusted_publishers.clone();
        let endorse_friendly = friendly_name.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(peer_endorse_interval)).await;
                let keys: Vec<String> = {
                    let tp = endorse_trusted.read().unwrap();
                    if tp.is_empty() {
                        continue;
                    }
                    tp.iter().cloned().collect()
                };
                let mut endorse = PeerAnnounce::new_endorse(
                    &endorse_node_id,
                    env!("CARGO_PKG_VERSION"),
                    &endorse_pubkey,
                    keys,
                    endorse_friendly.clone(),
                );
                endorse.sign_endorse(&endorse_signing_key);
                match serde_json::to_vec(&endorse) {
                    Ok(payload) => {
                        if let Err(e) = endorse_tx.send(payload).await {
                            eprintln!("\x1b[35m[WARN] Failed to queue PeerEndorse: {}\x1b[0m", e);
                        } else {
                            println!("\x1b[32m[ENDORSE] Broadcast PeerEndorse ({} keys)\x1b[0m", endorse.node_list.as_ref().map_or(0, |l| l.len()));
                        }
                    }
                    Err(e) => eprintln!("\x1b[35m[WARN] Failed to serialize PeerEndorse: {}\x1b[0m", e),
                }
            }
        });
    }

    // --- Re-bootstrap watchdog: if no GossipNotification in 3 minutes, re-join peers ---
    {
        let watchdog_last = last_notification_time.clone();
        let watchdog_shared = shared_sender.clone();
        let watchdog_peers_file = peers_file.clone();
        let watchdog_bootstrap_str = bootstrap_peer_ids_str.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(REBOOTSTRAP_TIMEOUT)).await;
                let last = watchdog_last.load(Ordering::Relaxed);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                if now - last >= REBOOTSTRAP_TIMEOUT {
                    println!("\x1b[33m[WATCHDOG] No gossip notifications for {}s, re-bootstrapping...\x1b[0m", now - last);

                    let mut peers: Vec<iroh::EndpointId> = watchdog_bootstrap_str
                        .split(',')
                        .filter(|s| !s.trim().is_empty())
                        .filter_map(|s| s.trim().parse().ok())
                        .collect();
                    let file_peers = load_known_peers(&watchdog_peers_file);
                    for p in file_peers {
                        if !peers.contains(&p) {
                            peers.push(p);
                        }
                    }

                    if peers.is_empty() {
                        println!("\x1b[33m[WATCHDOG] No known peers to re-bootstrap with\x1b[0m");
                    } else {
                        println!("\x1b[33m[WATCHDOG] Re-joining {} peers...\x1b[0m", peers.len());
                        let sender = watchdog_shared.read().await;
                        if let Err(e) = sender.join_peers(peers, None).await {
                            eprintln!("\x1b[35m[WARN] Re-bootstrap failed: {}\x1b[0m", e);
                        }
                    }
                }
            }
        });
    }

    // (receive task is spawned above via spawn_receive_task and re-spawned on reconnect)

    // --- Periodic health report ---
    {
        let h_sent = health_broadcasts_sent.clone();
        let h_failed = health_broadcasts_failed.clone();
        let h_last_ok = health_last_broadcast_ok.clone();
        let h_alive = health_broadcast_task_alive.clone();
        let h_shutdown = shutdown.clone();
        let h_tx = tx.clone();
        tokio::spawn(async move {
            let mut prev_sent: u64 = 0;
            let mut prev_failed: u64 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                if h_shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let sent = h_sent.load(Ordering::Relaxed);
                let failed = h_failed.load(Ordering::Relaxed);
                let last_ok = h_last_ok.load(Ordering::Relaxed);
                let alive = h_alive.load(Ordering::Relaxed);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                let since_last = now.saturating_sub(last_ok);
                let delta_sent = sent - prev_sent;
                let delta_failed = failed - prev_failed;
                let queue_len = h_tx.max_capacity() - h_tx.capacity();

                let status = if !alive {
                    "\x1b[1;31mBROADCAST TASK DEAD\x1b[0m"
                } else if since_last > REBOOTSTRAP_TIMEOUT {
                    "\x1b[1;31mSTALLED\x1b[0m"
                } else if delta_failed > 0 {
                    "\x1b[33mDEGRADED\x1b[0m"
                } else {
                    "\x1b[32mOK\x1b[0m"
                };

                println!(
                    "\x1b[90m[HEALTH] {} | sent={} failed={} | queue={}/1000 | last_ok={}s ago\x1b[0m",
                    status, delta_sent, delta_failed, queue_len, since_last,
                );

                prev_sent = sent;
                prev_failed = failed;
            }
        });
    }

    // --- ZMQ receive loop (blocking, runs in spawn_blocking) ---
    let zmq_bind_clone = zmq_bind.clone();
    let signing_key_clone = signing_key.clone();
    let pubkey_hex_clone = pubkey_hex.clone();

    let zmq_handle = tokio::task::spawn_blocking(move || {
        let ctx = zmq::Context::new();
        let pull_socket = ctx.socket(zmq::PAIR).unwrap();
        pull_socket.set_rcvtimeo(1000).unwrap(); // 1s recv timeout for shutdown checks
        pull_socket.set_sndtimeo(100).unwrap(); // 100ms send timeout for replies
        pull_socket.set_linger(0).unwrap();
        if let Err(e) = pull_socket.bind(&zmq_bind_clone) {
            eprintln!("\x1b[1;31m[FATAL] Cannot bind ZMQ socket on {}: {} (is another gossip-writer already running?)\x1b[0m", zmq_bind_clone, e);
            std::process::exit(1);
        }
        println!("  ZMQ PAIR socket bound on {}", zmq_bind_clone);

        let mut pending: Vec<PendingPing> = Vec::new();
        let mut last_flush = std::time::Instant::now();

        loop {
            if shutdown_zmq.load(Ordering::Relaxed) {
                // Flush remaining before shutdown
                if !pending.is_empty() {
                    flush_batch(
                        &mut pending,
                        &signing_key_clone,
                        &pubkey_hex_clone,
                        db.as_ref(),
                        &tx,
                        &pull_socket,
                    );
                }
                println!("  ZMQ thread: shutdown signal received.");
                break;
            }

            let mut msg = zmq::Message::new();
            match pull_socket.recv(&mut msg, 0) {
                Ok(_) => {
                    match parse_zmq_message(&msg) {
                        Ok(Some(ping)) => {
                            println!(
                                "\x1b[36m[ZMQ] Received: [{}] reason={} medium={}\x1b[0m",
                                ping.iri, ping.reason_str, ping.medium_str
                            );
                            pending.push(ping);
                        }
                        Ok(None) => {} // not a PodpingWrite, ignored
                        Err(e) => {
                            eprintln!("\x1b[35m[WARN] Error parsing ZMQ message: {}\x1b[0m", e);
                        }
                    }
                }
                Err(zmq::Error::EAGAIN) => {
                    // recv timeout - fall through to flush check
                }
                Err(e) => {
                    eprintln!("\x1b[35m[WARN] ZMQ recv error: {}\x1b[0m", e);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }

            // Flush batch every BATCH_INTERVAL_SECS
            if last_flush.elapsed() >= std::time::Duration::from_secs(BATCH_INTERVAL_SECS)
                && !pending.is_empty()
            {
                flush_batch(
                    &mut pending,
                    &signing_key_clone,
                    &pubkey_hex_clone,
                    db.as_ref(),
                    &tx,
                    &pull_socket,
                );
                last_flush = std::time::Instant::now();
            }
        }
    });

    // --- Wait for ctrl-c to shut down ---
    println!("\ngossip-writer running. Press Ctrl+C to stop.");
    signal::ctrl_c().await?;
    println!("\nShutting down...");

    // Signal the ZMQ thread to exit, then wait for it
    shutdown.store(true, Ordering::Relaxed);

    // Hard shutdown timeout — if graceful shutdown doesn't complete, force exit
    let shutdown_deadline = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(shutdown_deadline);

    tokio::select! {
        _ = async {
            let _ = zmq_handle.await;
            endpoint.close().await;
        } => {
            println!("[info] Clean shutdown complete.");
        }
        _ = &mut shutdown_deadline => {
            eprintln!("\x1b[33m[SHUTDOWN] Graceful shutdown timed out after 5s, forcing exit.\x1b[0m");
            std::process::exit(0);
        }
    }

    Ok(())
}

// Parse a ZMQ message and extract the IRI, medium, and reason.
// Returns None for non-PodpingWrite messages.
fn parse_zmq_message(
    msg: &zmq::Message,
) -> Result<Option<PendingPing>, Box<dyn std::error::Error>> {
    let message_reader =
        capnp::serialize::read_message(msg.as_ref(), capnp::message::ReaderOptions::new())?;
    let plexo_msg = message_reader.get_root::<plexo_message::Reader>()?;
    let payload_type = plexo_msg.get_type_name()?.to_str()?;

    if payload_type != "org.podcastindex.podping.hivewriter.PodpingWrite.capnp" {
        eprintln!("\x1b[35m[WARN] Ignoring non-PodpingWrite message: {}\x1b[0m", payload_type);
        return Ok(None);
    }

    let inner_reader = capnp::serialize::read_message(
        plexo_msg.get_payload()?,
        capnp::message::ReaderOptions::new(),
    )?;
    let podping = inner_reader.get_root::<podping_write::Reader>()?;

    let iri = podping.get_iri()?.to_str()?.to_string();
    let reason_enum = podping.get_reason()?;
    let medium_enum = podping.get_medium()?;

    let medium_raw = medium_enum as u16;
    let reason_raw = reason_enum as u16;
    let medium_str = notification::medium_to_string(medium_raw);
    let reason_str = notification::reason_to_string(reason_raw);

    Ok(Some(PendingPing {
        iri,
        medium_str,
        reason_str,
        medium_raw,
        reason_raw,
    }))
}

/// Split a list of IRIs into chunks that each produce a signed payload under MAX_GOSSIP_PAYLOAD.
/// Uses a greedy approach: build up each chunk by adding IRIs until the next one would exceed the limit.
fn split_iris_to_fit(
    pubkey_hex: &str,
    medium_str: &str,
    reason_str: &str,
    iris: &[String],
    signing_key: &ed25519_dalek::SigningKey,
) -> Vec<Vec<String>> {
    // Try all IRIs in one batch first (common case)
    {
        let mut notif = notification::GossipNotification::new(pubkey_hex, medium_str, reason_str, iris.to_vec());
        let payload = notif.sign(signing_key);
        if payload.len() <= MAX_GOSSIP_PAYLOAD {
            return vec![iris.to_vec()];
        }
    }

    // Need to split — estimate how many IRIs fit per chunk using the envelope overhead
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut current_chunk: Vec<String> = Vec::new();

    for iri in iris {
        current_chunk.push(iri.clone());

        // Test if this chunk still fits
        let mut test_notif = notification::GossipNotification::new(
            pubkey_hex, medium_str, reason_str, current_chunk.clone(),
        );
        let test_payload = test_notif.sign(signing_key);

        if test_payload.len() > MAX_GOSSIP_PAYLOAD {
            // This IRI pushed it over — remove it and finalize the chunk
            current_chunk.pop();
            if !current_chunk.is_empty() {
                chunks.push(current_chunk);
            }
            current_chunk = vec![iri.clone()];
        }
    }

    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
}

// Flush the pending batch: group by (medium, reason), build one GossipNotification
// per group, broadcast, then send a ZMQ reply for each IRI.
fn flush_batch(
    pending: &mut Vec<PendingPing>,
    signing_key: &ed25519_dalek::SigningKey,
    pubkey_hex: &str,
    db: Option<&archive::Archive>,
    tx: &mpsc::Sender<Vec<u8>>,
    socket: &zmq::Socket,
) {
    // Group IRIs by (medium_str, reason_str)
    let mut groups: HashMap<(&'static str, &'static str), Vec<String>> = HashMap::new();
    for ping in pending.iter() {
        groups.entry((ping.medium_str, ping.reason_str))
            .or_default()
            .push(ping.iri.clone());
    }

    // Build, sign, archive, and broadcast notifications per group.
    // If a group's payload exceeds MAX_GOSSIP_PAYLOAD, split IRIs into chunks.
    let mut broadcast_ok = false;
    for ((medium_str, reason_str), iris) in &groups {
        let total_iri_count = iris.len();

        // Build chunks of IRIs that fit within the payload limit
        let chunks = split_iris_to_fit(pubkey_hex, medium_str, reason_str, iris, signing_key);

        for (chunk_idx, chunk_iris) in chunks.iter().enumerate() {
            let chunk_count = chunk_iris.len();
            let mut notif = notification::GossipNotification::new(
                pubkey_hex, medium_str, reason_str, chunk_iris.clone(),
            );
            let signed_payload = notif.sign(signing_key);

            if let Some(db) = db {
                match db.store(
                    &signed_payload,
                    pubkey_hex,
                    medium_str,
                    reason_str,
                    notif.timestamp,
                    chunk_count,
                ) {
                    Ok(true) => println!("\x1b[36m[BATCH] Archived (new).\x1b[0m"),
                    Ok(false) => println!("\x1b[36m[BATCH] Archived (duplicate, skipped).\x1b[0m"),
                    Err(e) => eprintln!("\x1b[35m[WARN]  Archive error: {}\x1b[0m", e),
                }
            }

            match tx.try_send(signed_payload) {
                Ok(_) => {
                    if chunks.len() > 1 {
                        println!(
                            "\x1b[32m[BATCH] Broadcast chunk {}/{}: {} IRIs (medium={} reason={}, {} total)\x1b[0m",
                            chunk_idx + 1, chunks.len(), chunk_count, medium_str, reason_str, total_iri_count
                        );
                    } else {
                        println!(
                            "\x1b[32m[BATCH] Broadcast {} IRIs (medium={} reason={})\x1b[0m",
                            chunk_count, medium_str, reason_str
                        );
                    }
                    broadcast_ok = true;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    eprintln!("\x1b[1;31m[WARN]  Broadcast queue full — dropping batch. Gossip sender may be stalled.\x1b[0m");
                    broadcast_ok = true;
                }
                Err(e) => {
                    eprintln!("\x1b[35m[WARN]  Failed to queue batch for broadcast: {}\x1b[0m", e);
                }
            }
        }
    }

    // Send a ZMQ reply for each IRI so the front-end can dequeue them
    if broadcast_ok {
        let timestamp_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        for ping in pending.iter() {
            let medium_enum = PodpingMedium::try_from(ping.medium_raw).unwrap();
            let reason_enum = PodpingReason::try_from(ping.reason_raw).unwrap();

            if let Err(e) = send_zmq_reply(socket, &ping.iri, medium_enum, reason_enum, timestamp_secs, timestamp_ns) {
                eprintln!("\x1b[35m[WARN]  Failed to send reply for {}: {}\x1b[0m", ping.iri, e);
            }
        }
        println!(
            "\x1b[36m[BATCH] Sent {} ZMQ replies\x1b[0m",
            pending.len()
        );
    }

    pending.clear();
}

// Build and send a PodpingHiveTransaction reply for a single IRI
fn send_zmq_reply(
    socket: &zmq::Socket,
    iri: &str,
    medium_enum: PodpingMedium,
    reason_enum: PodpingReason,
    timestamp_secs: u64,
    timestamp_ns: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tx_message = capnp::message::Builder::new_default();
    {
        let mut hive_tx = tx_message.init_root::<podping_hive_transaction::Builder>();
        hive_tx.set_hive_tx_id("gossip");
        hive_tx.set_hive_block_num(timestamp_secs);
        let mut podpings = hive_tx.init_podpings(1);
        {
            let mut pp = podpings.reborrow().get(0);
            pp.set_medium(medium_enum);
            pp.set_reason(reason_enum);
            pp.set_timestamp_ns(timestamp_ns);
            pp.set_session_id(0);
            let mut iris = pp.init_iris(1);
            iris.set(0, iri);
        }
    }

    let mut tx_payload = Vec::new();
    capnp::serialize::write_message(&mut tx_payload, &tx_message)?;

    let mut plexo_builder = capnp::message::Builder::new_default();
    {
        let mut plexo = plexo_builder.init_root::<plexo_message::Builder>();
        plexo.set_type_name("org.podcastindex.podping.hivewriter.PodpingHiveTransaction.capnp");
        plexo.set_payload(capnp::data::Reader::from(tx_payload.as_slice()));
    }

    let mut reply_buf = Vec::new();
    capnp::serialize::write_message(&mut reply_buf, &plexo_builder)?;
    socket.send(&reply_buf, zmq::DONTWAIT)?;
    Ok(())
}

// Load trusted publisher public keys from a text file (one hex pubkey per line).
// Returns an empty HashSet if the file doesn't exist or is empty.
fn load_trusted_publishers(path: &str) -> HashSet<String> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return HashSet::new(),
    };
    contents
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

// Save trusted publishers to file (atomic overwrite)
fn save_trusted_publishers(path: &str, peers: &HashSet<String>) {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    if let Ok(mut f) = fs::File::create(path) {
        for key in peers {
            let _ = writeln!(f, "{}", key);
        }
    }
}

// Load known peers from a text file (one NodeId per line).
// Returns an empty vec if the file doesn't exist or can't be read.
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

// Save a peer's NodeId to the known-peers file if it's not already present
// and not our own node ID. Caps the file at MAX_KNOWN_PEERS entries,
// evicting the oldest (first) entries when full.
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

    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    if let Ok(mut f) = fs::File::create(path) {
        for p in &peers {
            let _ = writeln!(f, "{}", p);
        }
        println!("  Saved new peer to {}: {}", path, node_str);
    }
}

// Load a persistent iroh node key from `path`, or generate a new one and save it.
fn load_or_create_node_key(path: &str) -> Result<SecretKey, Box<dyn std::error::Error>> {
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
            .map_err(|_| format!("key file {} has invalid length", path))?;
        println!("  Loaded iroh node key from {}", path);
        Ok(SecretKey::from_bytes(&key_bytes))
    } else {
        let key = SecretKey::generate(&mut rand::rng());
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, key.to_bytes())?;
        println!("  Generated new iroh node key -> {}", path);
        Ok(key)
    }
}

/// Spawn an async task that processes incoming gossip events (PeerAnnounce, PeerEndorse,
/// GossipNotification, neighbor changes). Called on startup and again after reconnection.
fn spawn_receive_task(
    receiver: DttGossipReceiver,
    peers_file: String,
    my_node_id: iroh::EndpointId,
    trusted_publishers: Arc<RwLock<HashSet<String>>>,
    trusted_publishers_file: String,
    auto_trust_endorsements: bool,
    last_notification_time: Arc<AtomicU64>,
    peer_names: Arc<RwLock<HashMap<String, String>>>,
) {
    tokio::spawn(async move {
        while let Some(event) = receiver.next().await {
            match event {
                Ok(Event::Received(msg)) => {
                    // Try PeerAnnounce first
                    if let Ok(announce) = serde_json::from_slice::<PeerAnnounce>(&msg.content) {
                        if announce.msg_type == "peer_announce" {
                            if let Some(ref name) = announce.friendly_name {
                                println!("\x1b[33m[ANNOUNCE] PeerAnnounce from \"{}\" ({}) v{}\x1b[0m", name, announce.node_id, announce.version);
                                let mut names = peer_names.write().unwrap();
                                names.insert(announce.node_id.clone(), name.clone());
                            } else {
                                println!("\x1b[33m[ANNOUNCE] PeerAnnounce from {} v{}\x1b[0m", announce.node_id, announce.version);
                            }
                            if let Ok(node_id) = announce.node_id.parse() {
                                save_peer_if_new(&peers_file, &node_id, &my_node_id);
                            }
                        } else if announce.msg_type == "peer_endorse" {
                            if !auto_trust_endorsements {
                                continue;
                            }
                            // Verify signature
                            match announce.verify_endorse_signature() {
                                Ok(true) => {}
                                Ok(false) => {
                                    eprintln!("\x1b[35m[WARN] PeerEndorse without signature, ignoring\x1b[0m");
                                    continue;
                                }
                                Err(e) => {
                                    eprintln!("\x1b[35m[WARN] PeerEndorse bad signature: {e}\x1b[0m");
                                    continue;
                                }
                            }

                            let sender = match &announce.sender {
                                Some(s) => s.clone(),
                                None => continue,
                            };

                            let sender_display = match &announce.friendly_name {
                                Some(name) => format!("\"{}\" ({})", name, &sender[..8.min(sender.len())]),
                                None => sender[..8.min(sender.len())].to_string(),
                            };

                            // Check if the endorser is trusted
                            let is_trusted = {
                                let tp = trusted_publishers.read().unwrap();
                                tp.contains(&sender)
                            };

                            if !is_trusted {
                                println!("\x1b[33m[ENDORSE] PeerEndorse from untrusted sender {}, ignoring\x1b[0m", sender_display);
                                continue;
                            }

                            // Add endorsed keys
                            if let Some(ref node_list) = announce.node_list {
                                let mut added = 0;
                                {
                                    let mut tp = trusted_publishers.write().unwrap();
                                    for key in node_list {
                                        if tp.insert(key.clone()) {
                                            added += 1;
                                            println!("\x1b[32m[ENDORSE] Added trusted publisher {} (endorsed by {})\x1b[0m", &key[..8.min(key.len())], sender_display);
                                        }
                                    }
                                }
                                if added > 0 {
                                    let tp = trusted_publishers.read().unwrap();
                                    save_trusted_publishers(&trusted_publishers_file, &tp);
                                    println!("\x1b[32m[ENDORSE] {} new keys added, {} total trusted publishers\x1b[0m", added, tp.len());
                                }
                            }
                        }
                    } else {
                        // Fall back to GossipNotification
                        match serde_json::from_slice::<notification::GossipNotification>(&msg.content) {
                            Ok(notif) => {
                                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                                last_notification_time.store(now, Ordering::Relaxed);
                                println!(
                                    "\x1b[36m[GOSSIP] [{} IRIs] sender={} medium={} reason={}\x1b[0m",
                                    notif.iris.len(),
                                    &notif.sender[..8],
                                    notif.medium,
                                    notif.reason
                                );
                                for iri in notif.iris {
                                    println!("\x1b[36m[GOSSIP]   > {}\x1b[0m", iri);
                                }
                            }
                            Err(_) => {
                                eprintln!("\x1b[35m[WARN] unknown message format ({} bytes)\x1b[0m", msg.content.len());
                            }
                        }
                    }
                }
                Ok(Event::NeighborUp(node_id)) => {
                    let node_str = node_id.to_string();
                    let display = {
                        let names = peer_names.read().unwrap();
                        match names.get(&node_str) {
                            Some(name) => format!("\"{}\" ({})", name, node_id),
                            None => node_str,
                        }
                    };
                    println!("\x1b[32m[EVENT] NeighborUp: {display}\x1b[0m");
                    save_peer_if_new(&peers_file, &node_id, &my_node_id);
                }
                Ok(Event::NeighborDown(node_id)) => {
                    let node_str = node_id.to_string();
                    let display = {
                        let names = peer_names.read().unwrap();
                        match names.get(&node_str) {
                            Some(name) => format!("\"{}\" ({})", name, node_id),
                            None => node_str,
                        }
                    };
                    println!("\x1b[31m[EVENT] NeighborDown: {display}\x1b[0m");
                }
                Ok(Event::Lagged) => {
                    eprintln!("\x1b[35m[WARN] lagged — missed some messages\x1b[0m");
                }
                Err(e) => {
                    eprintln!("\x1b[35m[WARN] Gossip receiver error: {}\x1b[0m", e);
                    break;
                }
            }
        }
        println!("\x1b[33m[RECV] Gossip receive task ended.\x1b[0m");
    });
}