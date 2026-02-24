mod archive;
mod notification;

use bytes::Bytes;
use iroh::protocol::Router;
use iroh::SecretKey;
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};
use iroh_gossip::proto::TopicId;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::signal;
use tokio::sync::mpsc;
use futures_lite::StreamExt;
use iroh_gossip::net::{Event, GossipEvent};

// Cap'n Proto plexo message wrapper
pub mod plexo_message_capnp {
    include!("../plexo-schemas/built/dev/plexo/plexo_message_capnp.rs");
}
use crate::plexo_message_capnp::plexo_message;

// Podping schema types for deserialization
use podping_schemas::org::podcastindex::podping::podping_write_capnp::podping_write;

// Defaults
const DEFAULT_ZMQ_BIND: &str = "tcp://0.0.0.0:9998";
const DEFAULT_KEY_FILE: &str = "/data/gossip/iroh.key";
const DEFAULT_NODE_KEY_FILE: &str = "/data/gossip/iroh_node.key";
const DEFAULT_ARCHIVE_PATH: &str = "/data/gossip/archive.db";
const DEFAULT_KNOWN_PEERS_FILE: &str = "/data/gossip/known_peers.txt";
const TOPIC_STRING: &str = "gossipping/v1/all";

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

    println!("gossip-writer v{}", env!("CARGO_PKG_VERSION"));
    println!("  ZMQ bind:     {}", zmq_bind);
    println!("  Key file:     {}", key_file);
    println!("  Node key:     {}", node_key_file);
    println!("  Archive:      {}", archive_path);
    println!("  Peers file:   {}", peers_file);
    println!("  Topic:        {}", TOPIC_STRING);

    // --- Load or generate ed25519 signing key ---
    let signing_key = notification::load_or_generate_key(&key_file)?;
    let pubkey_hex = notification::pubkey_hex(&signing_key);
    println!("  Sender pubkey: {}", pubkey_hex);

    // --- Open SQLite archive ---
    let db = archive::Archive::open(&archive_path)?;
    println!("  Archive DB ready.");

    // --- Derive topic ID from SHA-256 of topic string ---
    let mut hasher = Sha256::new();
    hasher.update(TOPIC_STRING.as_bytes());
    let topic_hash: [u8; 32] = hasher.finalize().into();
    let topic_id = TopicId::from_bytes(topic_hash);
    println!("  Topic ID: {}", hex::encode(topic_hash));

    // --- Set up Iroh endpoint and gossip ---
    // Load or create a persistent iroh node key (separate from the ed25519-dalek signing key)
    let node_key = load_or_create_node_key(&node_key_file)?;
    let endpoint = iroh::Endpoint::builder()
        .secret_key(node_key)
        .discovery_n0()
        .bind()
        .await?;

    let my_node_id = endpoint.node_id();
    println!("  Iroh Node ID: {}", my_node_id);

    let gossip = Gossip::builder()
        .max_message_size(65536)
        .spawn(endpoint.clone())
        .await?;

    // Register gossip protocol with the router for incoming connections
    let router = Router::builder(endpoint.clone())
        .accept(GOSSIP_ALPN, gossip.clone())
        .spawn()
        .await?;

    // Parse bootstrap peer IDs (if any) and merge with known peers from file
    let mut bootstrap_peers: Vec<iroh::NodeId> = bootstrap_peer_ids_str
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
        println!("  No bootstrap peers configured; joining topic with no initial peers.");
    } else {
        println!("  Bootstrap peers: {:?}", bootstrap_peers);
    }

    // Subscribe to the topic (non-blocking, doesn't wait for peers)
    let topic = gossip.subscribe(topic_id, bootstrap_peers)?;
    let (gossip_sender, mut gossip_receiver) = topic.split();
    println!("  Joined gossip topic.");

    // --- mpsc channel: ZMQ thread -> async broadcast task ---
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1000);

    // --- Async broadcast task ---
    tokio::spawn(async move {
        while let Some(payload) = rx.recv().await {
            if let Err(e) = gossip_sender.broadcast(Bytes::from(payload)).await {
                eprintln!("  Gossip broadcast error: {}", e);
            }
        }
    });

    // --- Async receive task ---
    let recv_peers_file = peers_file.clone();
    let recv_my_node_id = my_node_id;
    tokio::spawn(async move {
        while let Some(event) = gossip_receiver.next().await {
            match event {
                Ok(Event::Gossip(GossipEvent::Received(msg))) => {
                    // Try to deserialize as GossipNotification
                    match serde_json::from_slice::<notification::GossipNotification>(&msg.content) {
                        Ok(notif) => {
                            println!(
                                "  GOSSIP RECV: [{} IRIs] sender={} medium={} reason={}",
                                notif.iris.len(),
                                &notif.sender[..8],
                                notif.medium,
                                notif.reason
                            );
                            for iri in notif.iris {
                                println!("    > {}", iri);
                            }
                        }
                        Err(_) => {
                            // If it's not a GossipNotification, it might be raw data or another format
                            println!("  GOSSIP RECV: unknown format ({} bytes)", msg.content.len());
                        }
                    }
                }
                Ok(Event::Gossip(GossipEvent::NeighborUp(node_id))) => {
                    println!("  [event] NeighborUp: {node_id}");
                    save_peer_if_new(&recv_peers_file, &node_id, &recv_my_node_id);
                }
                Ok(Event::Gossip(GossipEvent::NeighborDown(node_id))) => {
                    println!("  [event] NeighborDown: {node_id}");
                }
                Ok(Event::Gossip(GossipEvent::Joined(peers))) => {
                    println!("  [event] Joined topic with {} peer(s)", peers.len());
                }
                Ok(Event::Lagged) => {
                    eprintln!("  Gossip receiver lagged, some messages were missed.");
                }
                Err(e) => {
                    eprintln!("  Gossip receiver error: {}", e);
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
        let pull_socket = ctx.socket(zmq::PULL).unwrap();
        pull_socket.set_rcvtimeo(1000).unwrap(); // 1s recv timeout for shutdown checks
        pull_socket.set_linger(0).unwrap();
        pull_socket.bind(&zmq_bind_clone).unwrap();
        println!("  ZMQ PULL socket bound on {}", zmq_bind_clone);

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
                    ) {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("  Error processing ZMQ message: {}", e);
                        }
                    }
                }
                Err(zmq::Error::EAGAIN) => {
                    // recv timeout - loop back and check shutdown flag
                    continue;
                }
                Err(e) => {
                    eprintln!("  ZMQ recv error: {}", e);
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

    router.shutdown().await?;

    Ok(())
}

/// Process a single ZMQ message: deserialize Cap'n Proto, build notification,
/// sign, archive, and send to the broadcast channel.
fn process_message(
    msg: &zmq::Message,
    signing_key: &ed25519_dalek::SigningKey,
    pubkey_hex: &str,
    db: &archive::Archive,
    tx: &mpsc::Sender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read the PlexoMessage wrapper
    let message_reader =
        capnp::serialize::read_message(msg.as_ref(), capnp::message::ReaderOptions::new())?;
    let plexo_msg = message_reader.get_root::<plexo_message::Reader>()?;
    let payload_type = plexo_msg.get_type_name()?.to_str()?;

    // We only care about PodpingWrite messages
    if payload_type != "org.podcastindex.podping.hivewriter.PodpingWrite.capnp" {
        eprintln!("  Ignoring non-PodpingWrite message: {}", payload_type);
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
        "  Received: [{}] reason={} medium={}",
        iri, reason_str, medium_str
    );

    // Build and sign the Gossipping notification
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
        Ok(true) => println!("    Archived (new)."),
        Ok(false) => println!("    Archived (duplicate, skipped)."),
        Err(e) => eprintln!("    Archive error: {}", e),
    }

    // Send to the async broadcast task via mpsc channel
    match tx.blocking_send(signed_payload) {
        Ok(_) => println!("    Queued for gossip broadcast."),
        Err(e) => eprintln!("    Failed to queue for broadcast: {}", e),
    }

    Ok(())
}

/// Load known peers from a text file (one NodeId per line).
/// Returns an empty vec if the file doesn't exist or can't be read.
fn load_known_peers(path: &str) -> Vec<iroh::NodeId> {
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

/// Append a peer's NodeId to the known-peers file if it's not already present
/// and not our own node ID.
fn save_peer_if_new(path: &str, node_id: &iroh::NodeId, my_node_id: &iroh::NodeId) {
    if node_id == my_node_id {
        return;
    }
    let node_str = node_id.to_string();
    let existing = fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == node_str) {
        return;
    }
    use std::io::Write;
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", node_str);
        println!("  Saved new peer to {}: {}", path, node_str);
    }
}

/// Load a persistent iroh node key from `path`, or generate a new one and save it.
fn load_or_create_node_key(path: &str) -> Result<SecretKey, Box<dyn std::error::Error>> {
    if Path::new(path).exists() {
        let contents = fs::read_to_string(path)?;
        let key: SecretKey = contents.trim().parse()?;
        println!("  Loaded iroh node key from {}", path);
        Ok(key)
    } else {
        let key = SecretKey::generate(rand::rngs::OsRng);
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, key.to_string())?;
        println!("  Generated new iroh node key -> {}", path);
        Ok(key)
    }
}