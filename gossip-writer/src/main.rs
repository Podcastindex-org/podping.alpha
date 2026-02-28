mod archive;
mod notification;

use iroh::protocol::Router;
use iroh::SecretKey;
use iroh_gossip::net::Gossip;
use iroh_gossip::api::Event;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
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
use podping_schemas::org::podcastindex::podping::hivewriter::podping_hive_transaction_capnp::podping_hive_transaction;

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
}

// Canonical form for peer_endorse signing (alphabetical by serialized key name)
#[derive(Serialize)]
struct CanonicalPeerEndorse<'a> {
    node_id: &'a str,
    node_list: &'a Vec<String>,
    sender: &'a str,
    timestamp: u64,
    #[serde(rename = "type")]
    msg_type: &'a str,
    version: &'a str,
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
            sender: None,
            node_list: None,
            signature: None,
        }
    }

    fn new_endorse(node_id: &str, version: &str, sender: &str, endorsed_keys: Vec<String>) -> Self {
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
        }
    }

    fn canonical_endorse_bytes(&self) -> Vec<u8> {
        let canonical = CanonicalPeerEndorse {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    let trusted_publishers = Arc::new(RwLock::new(load_trusted_publishers(&trusted_publishers_file)));

    println!("gossip-writer v{}", env!("CARGO_PKG_VERSION"));
    println!("  ZMQ bind:     {}", zmq_bind);
    println!("  Key file:     {}", key_file);
    println!("  Node key:     {}", node_key_file);
    println!("  Archive:      {}", archive_path);
    println!("  Peers file:   {}", peers_file);
    println!("  Topic:        {}", TOPIC_STRING);
    println!("  Announce interval: {}s", peer_announce_interval);
    println!("  Endorse interval: {}s", peer_endorse_interval);
    println!("  Auto-trust endorsements: {}", auto_trust_endorsements);
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

    // --- Open SQLite archive ---
    let db = archive::Archive::open(&archive_path)?;
    println!("  Archive DB ready.");

    // --- Set up Iroh endpoint and gossip ---
    // Load or create a persistent iroh node key (separate from the ed25519-dalek signing key)
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

    // Register gossip protocol with the router for incoming connections
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

    // --- mpsc channel: ZMQ thread -> async broadcast task ---
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1000);
    let announce_tx = tx.clone();

    // --- Async broadcast task ---
    let broadcast_sender = gossip_sender.clone();
    tokio::spawn(async move {
        while let Some(payload) = rx.recv().await {
            if let Err(e) = broadcast_sender.broadcast(payload).await {
                eprintln!("\x1b[35m[WARN] Gossip broadcast error: {}\x1b[0m", e);
            }
        }
    });

    // --- Periodic PeerAnnounce task ---
    if peer_announce_interval > 0 {
        let announce_node_id = my_node_id.to_string();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(peer_announce_interval)).await;
                let announce = PeerAnnounce::new(&announce_node_id, env!("CARGO_PKG_VERSION"));
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

    // --- Async receive task ---
    let recv_peers_file = peers_file.clone();
    let recv_my_node_id = my_node_id;
    let recv_trusted = trusted_publishers.clone();
    let recv_trusted_file = trusted_publishers_file.clone();
    let recv_auto_trust = auto_trust_endorsements;
    tokio::spawn(async move {
        while let Some(event) = gossip_receiver.next().await {
            match event {
                Ok(Event::Received(msg)) => {
                    // Try PeerAnnounce first
                    if let Ok(announce) = serde_json::from_slice::<PeerAnnounce>(&msg.content) {
                        if announce.msg_type == "peer_announce" {
                            println!("\x1b[33m[ANNOUNCE] PeerAnnounce from {} v{}\x1b[0m", announce.node_id, announce.version);
                            if let Ok(node_id) = announce.node_id.parse() {
                                save_peer_if_new(&recv_peers_file, &node_id, &recv_my_node_id);
                            }
                        } else if announce.msg_type == "peer_endorse" {
                            if !recv_auto_trust {
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

                            // Check if the endorser is trusted
                            let is_trusted = {
                                let tp = recv_trusted.read().unwrap();
                                tp.contains(&sender)
                            };

                            if !is_trusted {
                                println!("\x1b[33m[ENDORSE] PeerEndorse from untrusted sender {}, ignoring\x1b[0m", &sender[..8.min(sender.len())]);
                                continue;
                            }

                            // Add endorsed keys
                            if let Some(ref node_list) = announce.node_list {
                                let mut added = 0;
                                {
                                    let mut tp = recv_trusted.write().unwrap();
                                    for key in node_list {
                                        if tp.insert(key.clone()) {
                                            added += 1;
                                            println!("\x1b[32m[ENDORSE] Added trusted publisher {} (endorsed by {})\x1b[0m", &key[..8.min(key.len())], &sender[..8.min(sender.len())]);
                                        }
                                    }
                                }
                                if added > 0 {
                                    let tp = recv_trusted.read().unwrap();
                                    save_trusted_publishers(&recv_trusted_file, &tp);
                                    println!("\x1b[32m[ENDORSE] {} new keys added, {} total trusted publishers\x1b[0m", added, tp.len());
                                }
                            }
                        }
                    } else {
                        // Fall back to GossipNotification
                        match serde_json::from_slice::<notification::GossipNotification>(&msg.content) {
                            Ok(notif) => {
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
                    println!("\x1b[32m[EVENT] NeighborUp: {node_id}\x1b[0m");
                    save_peer_if_new(&recv_peers_file, &node_id, &recv_my_node_id);
                }
                Ok(Event::NeighborDown(node_id)) => {
                    println!("\x1b[31m[EVENT] NeighborDown: {node_id}\x1b[0m");
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
    });

    // --- Shutdown flag for the blocking ZMQ thread ---
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_zmq = shutdown.clone();

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
        pull_socket.bind(&zmq_bind_clone).unwrap();
        println!("  ZMQ PAIR socket bound on {}", zmq_bind_clone);

        loop {
            if shutdown_zmq.load(Ordering::Relaxed) {
                println!("  ZMQ thread: shutdown signal received.");
                break;
            }

            let mut msg = zmq::Message::new();
            match pull_socket.recv(&mut msg, 0) {
                Ok(_) => {
                    match process_message(
                        &msg,
                        &signing_key_clone,
                        &pubkey_hex_clone,
                        &db,
                        &tx,
                        &pull_socket,
                    ) {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("\x1b[35m[WARN] Error processing ZMQ message: {}\x1b[0m", e);
                        }
                    }
                }
                Err(zmq::Error::EAGAIN) => {
                    // recv timeout - loop back and check shutdown flag
                    continue;
                }
                Err(e) => {
                    eprintln!("\x1b[35m[WARN] ZMQ recv error: {}\x1b[0m", e);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    });

    // --- Wait for ctrl-c to shut down ---
    println!("\ngossip-writer running. Press Ctrl+C to stop.");
    signal::ctrl_c().await?;
    println!("\nShutting down...");

    // Signal the ZMQ thread to exit, then wait for it
    shutdown.store(true, Ordering::Relaxed);
    let _ = zmq_handle.await;

    endpoint.close().await;

    Ok(())
}

// Process a single ZMQ message: deserialize Cap'n Proto, build notification,
// sign, archive, and send to the broadcast channel.
fn process_message(
    msg: &zmq::Message,
    signing_key: &ed25519_dalek::SigningKey,
    pubkey_hex: &str,
    db: &archive::Archive,
    tx: &mpsc::Sender<Vec<u8>>,
    socket: &zmq::Socket,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read the PlexoMessage wrapper
    let message_reader =
        capnp::serialize::read_message(msg.as_ref(), capnp::message::ReaderOptions::new())?;
    let plexo_msg = message_reader.get_root::<plexo_message::Reader>()?;
    let payload_type = plexo_msg.get_type_name()?.to_str()?;

    // We only care about PodpingWrite messages
    if payload_type != "org.podcastindex.podping.hivewriter.PodpingWrite.capnp" {
        eprintln!("\x1b[35m[WARN] Ignoring non-PodpingWrite message: {}\x1b[0m", payload_type);
        return Ok(());
    }

    // Extract the PodpingWrite from the plexo payload
    let inner_reader = capnp::serialize::read_message(
        plexo_msg.get_payload()?,
        capnp::message::ReaderOptions::new(),
    )?;
    let podping = inner_reader.get_root::<podping_write::Reader>()?;

    let iri = podping.get_iri()?.to_str()?.to_string();
    let reason_enum = podping.get_reason()?;
    let medium_enum = podping.get_medium()?;

    // Map capnp enums to Gossipping string values
    let reason_str = notification::reason_to_string(reason_enum as u16);
    let medium_str = notification::medium_to_string(medium_enum as u16);

    println!(
        "\x1b[36m[ZMQ] Received: [{}] reason={} medium={}\x1b[0m",
        iri, reason_str, medium_str
    );

    // Build and sign the Gossipping notification
    let iri_clone = iri.clone();
    let mut notif =
        notification::GossipNotification::new(pubkey_hex, medium_str, reason_str, vec![iri]);
    let signed_payload = notif.sign(signing_key);

    // Archive to SQLite (INSERT OR IGNORE by content hash)
    match db.store(
        &signed_payload,
        pubkey_hex,
        medium_str,
        reason_str,
        notif.timestamp,
        notif.iris.len(),
    ) {
        Ok(true) => println!("\x1b[36m[ZMQ]   Archived (new).\x1b[0m"),
        Ok(false) => println!("\x1b[36m[ZMQ]   Archived (duplicate, skipped).\x1b[0m"),
        Err(e) => eprintln!("\x1b[35m[WARN]  Archive error: {}\x1b[0m", e),
    }

    // Send to the async broadcast task via mpsc channel
    match tx.blocking_send(signed_payload) {
        Ok(_) => {
            println!("\x1b[32m[ZMQ]   Queued for gossip broadcast.\x1b[0m");

            // Build a PodpingHiveTransaction reply so podping can remove the IRI from the queue
            let timestamp_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let timestamp_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;

            // Build inner PodpingHiveTransaction
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
                    iris.set(0, &iri_clone);
                }
            }

            // Serialize the inner message
            let mut tx_payload = Vec::new();
            capnp::serialize::write_message(&mut tx_payload, &tx_message)?;

            // Wrap in PlexoMessage
            let mut plexo_builder = capnp::message::Builder::new_default();
            {
                let mut plexo = plexo_builder.init_root::<plexo_message::Builder>();
                plexo.set_type_name("org.podcastindex.podping.hivewriter.PodpingHiveTransaction.capnp");
                plexo.set_payload(capnp::data::Reader::from(tx_payload.as_slice()));
            }

            // Serialize and send the reply
            let mut reply_buf = Vec::new();
            capnp::serialize::write_message(&mut reply_buf, &plexo_builder)?;
            match socket.send(&reply_buf, zmq::DONTWAIT) {
                Ok(_) => println!("\x1b[36m[ZMQ]   Sent PodpingHiveTransaction reply (gossip).\x1b[0m"),
                Err(e) => eprintln!("\x1b[35m[WARN]  Failed to send reply: {}\x1b[0m", e),
            }
        }
        Err(e) => eprintln!("\x1b[35m[WARN]  Failed to queue for broadcast: {}\x1b[0m", e),
    }

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