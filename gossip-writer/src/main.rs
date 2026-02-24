mod archive;
mod notification;

use bytes::Bytes;
use iroh::protocol::Router;
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};
use iroh_gossip::proto::TopicId;
use sha2::{Digest, Sha256};
use std::env;
use tokio::signal;
use tokio::sync::mpsc;

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
const DEFAULT_ARCHIVE_PATH: &str = "/data/gossip/archive.db";
const TOPIC_STRING: &str = "gossipping/v1/all";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --- Configuration from environment ---
    let zmq_bind = env::var("ZMQ_BIND_ADDR").unwrap_or_else(|_| DEFAULT_ZMQ_BIND.to_string());
    let key_file = env::var("IROH_SECRET_FILE").unwrap_or_else(|_| DEFAULT_KEY_FILE.to_string());
    let archive_path =
        env::var("ARCHIVE_PATH").unwrap_or_else(|_| DEFAULT_ARCHIVE_PATH.to_string());
    let bootstrap_peer_ids_str = env::var("BOOTSTRAP_PEER_IDS").unwrap_or_default();

    println!("gossip-writer v{}", env!("CARGO_PKG_VERSION"));
    println!("  ZMQ bind:     {}", zmq_bind);
    println!("  Key file:     {}", key_file);
    println!("  Archive:      {}", archive_path);
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
    // Use iroh's own key for the endpoint (generated fresh - the ed25519-dalek key
    // is for notification signing, the iroh endpoint has its own identity)
    let endpoint = iroh::Endpoint::builder()
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

    // Parse bootstrap peer IDs (if any)
    let bootstrap_peers: Vec<iroh::NodeId> = bootstrap_peer_ids_str
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    if bootstrap_peers.is_empty() {
        println!("  No bootstrap peers configured; joining topic with no initial peers.");
    } else {
        println!("  Bootstrap peers: {:?}", bootstrap_peers);
    }

    // Subscribe to the topic (non-blocking, doesn't wait for peers)
    let topic = gossip.subscribe(topic_id, bootstrap_peers)?;
    let (gossip_sender, _gossip_receiver) = topic.split();
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

    // --- ZMQ receive loop (blocking, runs in spawn_blocking) ---
    let zmq_bind_clone = zmq_bind.clone();
    let signing_key_clone = signing_key.clone();
    let pubkey_hex_clone = pubkey_hex.clone();

    tokio::task::spawn_blocking(move || {
        let ctx = zmq::Context::new();
        let pull_socket = ctx.socket(zmq::PULL).unwrap();
        pull_socket.set_rcvtimeo(1000).unwrap(); // 1s recv timeout for shutdown checks
        pull_socket.set_linger(0).unwrap();
        pull_socket.bind(&zmq_bind_clone).unwrap();
        println!("  ZMQ PULL socket bound on {}", zmq_bind_clone);

        loop {
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
                    // recv timeout - just loop back (allows checking for shutdown)
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
