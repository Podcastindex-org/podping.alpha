mod archive;
mod sse;

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use std::io::Write;
use std::os::unix::io::FromRawFd;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use iroh::protocol::Router;
use iroh::SecretKey;
use iroh_gossip::api::Event;
use iroh_gossip::net::Gossip;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use distributed_topic_tracker::{AutoDiscoveryGossip, RecordPublisher, TopicId as DttTopicId,
    GossipSender as DttGossipSender, GossipReceiver as DttGossipReceiver};

const TOPIC_STRING: &str = "gossipping/v1/all";
const DEFAULT_NODE_KEY_FILE: &str = "gossip_listener_node.key";
const DEFAULT_KNOWN_PEERS_FILE: &str = "gossip_listener_known_peers.txt";
const MAX_KNOWN_PEERS: usize = 15;
const DEFAULT_DHT_SECRET: &str = "podping_gossip_default_secret";
const DEFAULT_TRUSTED_PUBLISHERS_FILE: &str = "trusted_publishers.txt";
const DEFAULT_ARCHIVE_PATH: &str = "listener_archive.db";
const DEFAULT_PEER_ENDORSE_INTERVAL: u64 = 45;
const REBOOTSTRAP_TIMEOUT: u64 = 180;
const RECONNECT_AFTER_FAILURES: u64 = 5;
const BROADCAST_TIMEOUT_SECS: u64 = 10;
const ARCHIVE_SYNC_ALPN: &[u8] = b"/podping-archive-sync/1";
const DEFAULT_SSE_BIND_ADDR: &str = "0.0.0.0:8089";
const DEFAULT_SSE_BUFFER_SIZE: usize = 1000;

//Structs ------------------------------------------------------------------------------------------

// PeerAnnounce: periodic node ID announcement over gossip so that other nodes
// have a chance to see who is connected to the topic and save those as
// bootstrap nodes for later use.
// Also used for peer_endorse messages (trust propagation).
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

//GossipNotification to match existing writer format
#[derive(Debug, Clone, Deserialize, Serialize)]
struct GossipNotification {
    version: String,
    sender: String,
    timestamp: u64,
    medium: String,
    reason: String,
    iris: Vec<String>,
    signature: Option<String>,
}

// Canonical form with fields in alphabetical order for signature verification
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
        serde_json::to_vec(&canonical).expect("Canonical JSON serialization should not fail")
    }

    fn verify_signature(&self) -> Result<bool, String> {
        let sig_hex = match &self.signature {
            Some(s) => s,
            None => return Ok(false),
        };

        let pubkey_bytes: [u8; 32] = hex::decode(&self.sender)
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

        let canonical = self.canonical_bytes();
        match verifying_key.verify(&canonical, &signature) {
            Ok(()) => Ok(true),
            Err(e) => Err(format!("Signature verification failed: {e}")),
        }
    }
}

//ArchiveSyncHandler - serves archived notifications to peers over a custom ALPN protocol
#[derive(Debug, Clone)]
struct ArchiveSyncHandler {
    db: Arc<Mutex<archive::Archive>>,
}

impl iroh::protocol::ProtocolHandler for ArchiveSyncHandler {
    async fn accept(
        &self,
        connection: iroh::endpoint::Connection,
    ) -> Result<(), iroh::protocol::AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;

        // Read 8-byte big-endian u64 since_timestamp
        let mut ts_buf = [0u8; 8];
        recv.read_exact(&mut ts_buf).await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("read timestamp: {e}")))?;
        let since = u64::from_be_bytes(ts_buf);

        println!(
            "\x1b[36m[SYNC] Peer requested catch-up since {} ({}s ago)\x1b[0m",
            since,
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs().saturating_sub(since)
        );

        let payloads = {
            let db = match self.db.lock() {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("\x1b[35m[SYNC] Archive lock poisoned: {}\x1b[0m", e);
                    return Ok(());
                }
            };
            db.messages_since(since).unwrap_or_default()
        };

        // Cap response to avoid OOM on very large archives
        let payloads: Vec<_> = if payloads.len() > 50_000 {
            println!("\x1b[33m[SYNC] Capping response at 50000 of {} messages\x1b[0m", payloads.len());
            payloads.into_iter().take(50_000).collect()
        } else {
            payloads
        };

        for payload in &payloads {
            let len = (payload.len() as u32).to_be_bytes();
            send.write_all(&len).await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("write len: {e}")))?;
            send.write_all(payload).await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("write payload: {e}")))?;
        }

        send.finish()?;
        println!("\x1b[36m[SYNC] Sent {} archived notifications\x1b[0m", payloads.len());

        Ok(())
    }
}

//Catch-up client - connects to a peer and downloads missed notifications
async fn run_catchup(
    endpoint: &iroh::Endpoint,
    since: u64,
    peers_file: &str,
    bootstrap_str: &str,
    neighbor_rx: &mut tokio::sync::mpsc::Receiver<iroh::EndpointId>,
    trusted_publishers: &Arc<RwLock<HashSet<String>>>,
    last_notification_time: &Arc<AtomicU64>,
    db: &Option<Arc<Mutex<archive::Archive>>>,
    sse_tx: &Option<tokio::sync::broadcast::Sender<String>>,
) {
    println!("\x1b[36m[CATCHUP] Starting catch-up from {}s ago\x1b[0m",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs().saturating_sub(since));

    // Build candidate peer list
    let mut candidate_peers: Vec<iroh::EndpointId> = bootstrap_str
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let file_peers = load_known_peers(peers_file);
    for p in file_peers {
        if !candidate_peers.contains(&p) {
            candidate_peers.push(p);
        }
    }

    // Try each known peer
    let mut conn: Option<iroh::endpoint::Connection> = None;
    for peer_id in &candidate_peers {
        println!("\x1b[36m[CATCHUP] Trying peer {}...\x1b[0m", peer_id);
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            endpoint.connect(*peer_id, ARCHIVE_SYNC_ALPN),
        ).await {
            Ok(Ok(c)) => {
                println!("\x1b[32m[CATCHUP] Connected to {}\x1b[0m", peer_id);
                conn = Some(c);
                break;
            }
            Ok(Err(e)) => println!("\x1b[33m[CATCHUP] Peer {} refused: {}\x1b[0m", peer_id, e),
            Err(_) => println!("\x1b[33m[CATCHUP] Peer {} timed out\x1b[0m", peer_id),
        }
    }

    // Fallback: wait for first NeighborUp
    if conn.is_none() {
        println!("\x1b[33m[CATCHUP] No known peers responded, waiting for NeighborUp...\x1b[0m");
        match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            neighbor_rx.recv(),
        ).await {
            Ok(Some(peer_id)) => {
                println!("\x1b[36m[CATCHUP] Trying NeighborUp peer {}...\x1b[0m", peer_id);
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    endpoint.connect(peer_id, ARCHIVE_SYNC_ALPN),
                ).await {
                    Ok(Ok(c)) => {
                        println!("\x1b[32m[CATCHUP] Connected to {}\x1b[0m", peer_id);
                        conn = Some(c);
                    }
                    Ok(Err(e)) => println!("\x1b[35m[CATCHUP] NeighborUp peer refused: {}\x1b[0m", e),
                    Err(_) => println!("\x1b[35m[CATCHUP] NeighborUp peer timed out\x1b[0m"),
                }
            }
            Ok(None) => println!("\x1b[35m[CATCHUP] NeighborUp channel closed\x1b[0m"),
            Err(_) => println!("\x1b[35m[CATCHUP] Timed out waiting for NeighborUp\x1b[0m"),
        }
    }

    let conn = match conn {
        Some(c) => c,
        None => {
            println!("\x1b[35m[CATCHUP] Catch-up failed — continuing with live-only mode\x1b[0m");
            return;
        }
    };

    // Open bidi stream and send since_timestamp
    let (mut send, mut recv) = match conn.open_bi().await {
        Ok(streams) => streams,
        Err(e) => {
            eprintln!("\x1b[35m[CATCHUP] Failed to open stream: {}\x1b[0m", e);
            return;
        }
    };

    if let Err(e) = send.write_all(&since.to_be_bytes()).await {
        eprintln!("\x1b[35m[CATCHUP] Failed to send timestamp: {}\x1b[0m", e);
        return;
    }
    let _ = send.finish();

    // Read length-prefixed payloads
    let mut count = 0u64;
    loop {
        let mut len_buf = [0u8; 4];
        match recv.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(_) => break,
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 || len > 10_000_000 {
            break;
        }

        let mut payload = vec![0u8; len];
        if recv.read_exact(&mut payload).await.is_err() {
            break;
        }

        if let Ok(notif) = serde_json::from_slice::<GossipNotification>(&payload) {
            let tp = trusted_publishers.read().unwrap();
            if tp.is_empty() || tp.contains(&notif.sender) {
                let sender_display = notif.sender[..8.min(notif.sender.len())].to_string();
                let json = print_notification(&notif, &sender_display);
                if let Some(ref tx) = sse_tx {
                    let _ = tx.send(json);
                }
                if let Some(ref db_arc) = db {
                    let db_lock = db_arc.lock().unwrap();
                    match db_lock.store(
                        &payload,
                        &notif.sender,
                        &notif.medium,
                        &notif.reason,
                        notif.timestamp,
                        notif.iris.len(),
                    ) {
                        Ok(true) => println!("\x1b[36m[ARCHIVE] Stored (catchup)\x1b[0m"),
                        Ok(false) => {}
                        Err(e) => eprintln!("\x1b[35m[WARN] Archive error: {}\x1b[0m", e),
                    }
                }
                count += 1;
            }
        }
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    last_notification_time.store(now, Ordering::Relaxed);
    println!("\x1b[32m[CATCHUP] Catch-up complete: received {} notifications\x1b[0m", count);
}

//Main ---------------------------------------------------------------------------------------------
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Write tracing output to fd 3 only if TRACE_FD3=1 and fd 3 is a pipe or
    // regular file, otherwise stderr. This avoids inherited sockets or other
    // incompatible fds that reject write() with EINVAL.
    // Usage: TRACE_FD3=1 RUST_LOG=debug ./gossip-listener 3>trace.log
    let trace_writer: Box<dyn std::io::Write + Send + Sync> = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        let fd3_ok = env::var("TRACE_FD3").as_deref() == Ok("1")
            && libc::fstat(3, &mut stat) == 0
            && {
                let ft = stat.st_mode & libc::S_IFMT;
                ft == libc::S_IFIFO || ft == libc::S_IFREG
            };
        if fd3_ok {
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
            eprintln!("\x1b[1;31m[PANIC] This is an upstream bug, not a gossip-listener issue. Process will restart via Docker.\x1b[0m");
        } else if is_actor_shutdown {
            eprintln!("\x1b[33m[SHUTDOWN] DTT actor stopped (normal during shutdown)\x1b[0m");
        } else {
            eprintln!("\x1b[1;31m[PANIC] at {}: {}\x1b[0m", location, payload);
            let bt = std::backtrace::Backtrace::force_capture();
            eprintln!("{}", bt);
        }
    }));
    // END WORKAROUND

    println!("gossip-listener v{}\n", env!("CARGO_PKG_VERSION"));

    // Configure from the environment
    let bootstrap_peer_ids_str = env::var("BOOTSTRAP_PEER_IDS").unwrap_or_default();
    let node_key_file =
        env::var("IROH_NODE_KEY_FILE").unwrap_or_else(|_| DEFAULT_NODE_KEY_FILE.to_string());
    let peers_file =
        env::var("KNOWN_PEERS_FILE").unwrap_or_else(|_| DEFAULT_KNOWN_PEERS_FILE.to_string());
    let dht_initial_secret = env::var("DHT_INITIAL_SECRET")
        .unwrap_or_else(|_| DEFAULT_DHT_SECRET.to_string());
    let trusted_publishers_file =
        env::var("TRUSTED_PUBLISHERS_FILE").unwrap_or_else(|_| DEFAULT_TRUSTED_PUBLISHERS_FILE.to_string());
    let peer_endorse_interval: u64 = env::var("PEER_ENDORSE_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PEER_ENDORSE_INTERVAL);
    let archive_enabled = env::var("ARCHIVE_ENABLED")
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let archive_path =
        env::var("ARCHIVE_PATH").unwrap_or_else(|_| DEFAULT_ARCHIVE_PATH.to_string());

    let trusted_publishers = Arc::new(RwLock::new(load_trusted_publishers(&trusted_publishers_file)));

    // Optionally open SQLite archive
    let db: Option<Arc<Mutex<archive::Archive>>> = if archive_enabled {
        match archive::Archive::open(&archive_path) {
            Ok(a) => {
                println!("  Archive DB ready: {}", archive_path);
                Some(Arc::new(Mutex::new(a)))
            }
            Err(e) => {
                eprintln!("\x1b[35m[WARN] Failed to open archive DB: {}\x1b[0m", e);
                None
            }
        }
    } else {
        None
    };

    let catchup_enabled = env::var("CATCHUP_ENABLED")
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    let sse_enabled = env::var("SSE_ENABLED")
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let sse_bind_addr: std::net::SocketAddr = env::var("SSE_BIND_ADDR")
        .unwrap_or_else(|_| DEFAULT_SSE_BIND_ADDR.to_string())
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("\x1b[35m[WARN] Invalid SSE_BIND_ADDR, using default: {}\x1b[0m", e);
            DEFAULT_SSE_BIND_ADDR.parse().unwrap()
        });
    let sse_buffer_size: usize = env::var("SSE_BUFFER_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_SSE_BUFFER_SIZE);

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

    println!("  Topic: \"{}\"", TOPIC_STRING);
    println!("  DHT discovery: enabled");
    println!("  Archive:      {}", if archive_enabled { &archive_path } else { "disabled" });
    println!("  Catch-up:     {}", if catchup_enabled { "enabled" } else { "disabled" });
    println!("  SSE:          {}", if sse_enabled { format!("{}", sse_bind_addr) } else { "disabled".to_string() });
    if let Some(ref name) = friendly_name {
        println!("  Friendly name: {}", name);
    }
    println!("  Trusted publishers file: {}", trusted_publishers_file);
    {
        let tp = trusted_publishers.read().unwrap();
        if tp.is_empty() {
            println!("  Trusted publisher filter: disabled (accepting all)");
        } else {
            println!("  Trusted publisher filter: {} senders", tp.len());
        }
    }

    // Start SSE server if enabled
    let sse_tx: Option<tokio::sync::broadcast::Sender<String>> = if sse_enabled {
        Some(sse::start_sse_server(sse_bind_addr, sse_buffer_size))
    } else {
        None
    };

    //Set up Iroh context
    let node_key = load_or_create_node_key(&node_key_file)?;
    let node_key_bytes = node_key.to_bytes();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(node_key)
        .bind()
        .await?;

    // Create ed25519 signing key for peer_endorse messages (derived from node key)
    let signing_key = SigningKey::from_bytes(&node_key_bytes);
    let pubkey_hex = hex::encode(signing_key.verifying_key().to_bytes());

    //Self assign an Iroh node id
    let my_node_id = endpoint.id();
    let peer_names: Arc<RwLock<HashMap<String, String>>> = Arc::new(RwLock::new(HashMap::new()));
    println!("  Iroh Node ID: {}", my_node_id);
    println!("  Sender pubkey: {}", pubkey_hex);

    //Launch Iroh modules
    let gossip = Gossip::builder()
        .max_message_size(65536)
        .spawn(endpoint.clone());
    let mut router_builder = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone());

    if let Some(ref db_arc) = db {
        let handler = ArchiveSyncHandler { db: db_arc.clone() };
        router_builder = router_builder.accept(ARCHIVE_SYNC_ALPN, handler);
        println!("  Archive sync: serving on ALPN {}", std::str::from_utf8(ARCHIVE_SYNC_ALPN).unwrap());
    }

    let router = router_builder.spawn();

    // Bootstrap this node over DHT
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

    // Shared sender so all tasks use the same sender (and reconnect can replace it)
    let shared_sender: Arc<tokio::sync::RwLock<DttGossipSender>> =
        Arc::new(tokio::sync::RwLock::new(gossip_sender));
    let broadcast_failures = Arc::new(AtomicU64::new(0));
    let notifications_received = Arc::new(AtomicU64::new(0));
    let reconnect_requested = Arc::new(AtomicBool::new(false));
    let reconnect_notify = Arc::new(Notify::new());
    let receive_generation = Arc::new(AtomicU64::new(0));

    //Joining peers from the known peers file serves as fallback/insurance if DHT no work
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

    // --- Re-bootstrap watchdog timer ---
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let last_notification_time = Arc::new(AtomicU64::new(now_secs));

    //Periodically announce ourselves to the topic for the bootstrapping benefit of others
    let peer_announce_interval: u64 = env::var("PEER_ANNOUNCE_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    println!("  Announce interval: {}s", peer_announce_interval);
    println!("  Endorse interval: {}s", peer_endorse_interval);

    if peer_announce_interval > 0 {
        let announce_shared = shared_sender.clone();
        let announce_failures = broadcast_failures.clone();
        let announce_node_id = my_node_id.to_string();
        let announce_friendly = friendly_name.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(peer_announce_interval)).await;
                let announce = PeerAnnounce::new(&announce_node_id, env!("CARGO_PKG_VERSION"), announce_friendly.clone());
                match serde_json::to_vec(&announce) {
                    Ok(payload) => {
                        let sender = announce_shared.read().await;
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(BROADCAST_TIMEOUT_SECS),
                            sender.broadcast(payload),
                        ).await {
                            Ok(Ok(_)) => {
                                announce_failures.store(0, Ordering::Relaxed);
                                println!("[info] Broadcast PeerAnnounce for {}", announce_node_id);
                            }
                            Ok(Err(e)) => {
                                let count = announce_failures.fetch_add(1, Ordering::Relaxed) + 1;
                                if count <= 3 {
                                    eprintln!("\x1b[35m[WARN] Failed to broadcast PeerAnnounce: {}\x1b[0m", e);
                                }
                            }
                            Err(_) => {
                                let count = announce_failures.fetch_add(1, Ordering::Relaxed) + 1;
                                if count <= 3 {
                                    eprintln!("\x1b[35m[WARN] PeerAnnounce broadcast timed out ({}s)\x1b[0m", BROADCAST_TIMEOUT_SECS);
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("[error] Failed to serialize PeerAnnounce: {}", e),
                }
            }
        });
    }

    // --- Periodic PeerEndorse broadcast task ---
    if peer_endorse_interval > 0 {
        let endorse_shared = shared_sender.clone();
        let endorse_failures = broadcast_failures.clone();
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
                        let sender = endorse_shared.read().await;
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(BROADCAST_TIMEOUT_SECS),
                            sender.broadcast(payload),
                        ).await {
                            Ok(Ok(_)) => {
                                endorse_failures.store(0, Ordering::Relaxed);
                                println!("[info] Broadcast PeerEndorse ({} keys)", endorse.node_list.as_ref().map_or(0, |l| l.len()));
                            }
                            Ok(Err(e)) => {
                                let count = endorse_failures.fetch_add(1, Ordering::Relaxed) + 1;
                                if count <= 3 {
                                    eprintln!("\x1b[35m[WARN] Failed to broadcast PeerEndorse: {}\x1b[0m", e);
                                }
                            }
                            Err(_) => {
                                let count = endorse_failures.fetch_add(1, Ordering::Relaxed) + 1;
                                if count <= 3 {
                                    eprintln!("\x1b[35m[WARN] PeerEndorse broadcast timed out ({}s)\x1b[0m", BROADCAST_TIMEOUT_SECS);
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("[error] Failed to serialize PeerEndorse: {}", e),
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
        let watchdog_failures = broadcast_failures.clone();
        tokio::spawn(async move {
            let mut stall_count: u64 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(REBOOTSTRAP_TIMEOUT)).await;
                let last = watchdog_last.load(Ordering::Relaxed);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                if now - last >= REBOOTSTRAP_TIMEOUT {
                    stall_count += 1;
                    println!("\x1b[33m[WATCHDOG] No gossip notifications for {}s, re-bootstrapping (stall #{})...\x1b[0m", now - last, stall_count);

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

                    // If join_peers hasn't helped after two consecutive stalls, escalate to
                    // a full reconnect by tripping the reconnect monitor's threshold
                    if stall_count >= 2 {
                        println!("\x1b[33m[WATCHDOG] Stall persists after re-bootstrap, triggering full reconnect...\x1b[0m");
                        watchdog_failures.store(RECONNECT_AFTER_FAILURES, Ordering::Relaxed);
                        stall_count = 0;
                    }
                } else {
                    stall_count = 0;
                }
            }
        });
    }

    // --- Reconnection monitor: watches for consecutive broadcast failures ---
    {
        let reconnect_failures = broadcast_failures.clone();
        let reconnect_requested = reconnect_requested.clone();
        let reconnect_notify = reconnect_notify.clone();
        let reconnect_shared = shared_sender.clone();
        let reconnect_endpoint = endpoint.clone();
        let reconnect_node_key_bytes = node_key_bytes;
        let reconnect_dht_secret = dht_initial_secret.clone();
        let reconnect_peers_file = peers_file.clone();
        let reconnect_my_node_id = my_node_id;
        let reconnect_trusted = trusted_publishers.clone();
        let reconnect_trusted_file = trusted_publishers_file.clone();
        let reconnect_last_notif = last_notification_time.clone();
        let reconnect_peer_names = peer_names.clone();
        let reconnect_db = db.clone();
        let reconnect_sse_tx = sse_tx.clone();
        let reconnect_notif_count = notifications_received.clone();
        let reconnect_receive_generation = receive_generation.clone();
        tokio::spawn(async move {
            // Keep the router alive in this task; on reconnect we replace it
            // (dropping the old router aborts its accept loop without closing the endpoint)
            let mut _current_router = router;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                    _ = reconnect_notify.notified() => {}
                }
                let failures = reconnect_failures.load(Ordering::Relaxed);
                let requested = reconnect_requested.swap(false, Ordering::Relaxed);
                if failures >= RECONNECT_AFTER_FAILURES || requested {
                    eprintln!("\x1b[1;31m[RECONNECT] {} consecutive broadcast failures — reconnecting gossip topic...\x1b[0m", failures);

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
                    // Re-register the new gossip (and archive sync if active) with a fresh Router
                    let mut new_router_builder = Router::builder(reconnect_endpoint.clone())
                        .accept(iroh_gossip::ALPN, new_gossip.clone());
                    if let Some(ref db_arc) = reconnect_db {
                        let handler = ArchiveSyncHandler { db: db_arc.clone() };
                        new_router_builder = new_router_builder.accept(ARCHIVE_SYNC_ALPN, handler);
                    }
                    // Replace the router: dropping the old one stops its accept loop,
                    // the new one routes incoming connections to the fresh actor
                    _current_router = new_router_builder.spawn();
                    match new_gossip
                        .subscribe_and_join_with_auto_discovery_no_wait(publisher)
                        .await
                    {
                        Ok(new_topic) => match new_topic.split().await {
                            Ok((new_sender, new_receiver)) => {
                                // Replace the shared sender
                                {
                                    let mut sender_guard = reconnect_shared.write().await;
                                    *sender_guard = new_sender;
                                }
                                reconnect_failures.store(0, Ordering::Relaxed);

                                // Spawn a fresh receive task
                                spawn_receive_task(
                                    new_receiver,
                                    reconnect_peers_file.clone(),
                                    reconnect_my_node_id,
                                    reconnect_trusted.clone(),
                                    reconnect_trusted_file.clone(),
                                    reconnect_last_notif.clone(),
                                    reconnect_db.clone(),
                                    Arc::new(Mutex::new(None)),
                                    reconnect_peer_names.clone(),
                                    reconnect_sse_tx.clone(),
                                    reconnect_notif_count.clone(),
                                    reconnect_failures.clone(),
                                    reconnect_requested.clone(),
                                    reconnect_notify.clone(),
                                    reconnect_receive_generation.clone(),
                                    reconnect_receive_generation.fetch_add(1, Ordering::Relaxed) + 1,
                                );

                                println!("\x1b[32m[RECONNECT] Gossip topic reconnected successfully.\x1b[0m");
                            }
                            Err(e) => {
                                eprintln!("\x1b[1;31m[RECONNECT] Failed to split topic: {}. Will retry.\x1b[0m", e);
                                reconnect_failures.store(0, Ordering::Relaxed);
                            }
                        },
                        Err(e) => {
                            eprintln!("\x1b[1;31m[RECONNECT] Failed to re-subscribe: {}. Will retry.\x1b[0m", e);
                            reconnect_failures.store(0, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
    }

    // --- Periodic health report ---
    {
        let h_notifs = notifications_received.clone();
        let h_failures = broadcast_failures.clone();
        let h_last_notif = last_notification_time.clone();
        tokio::spawn(async move {
            let mut prev_notifs: u64 = 0;
            let mut prev_failures: u64 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let notifs = h_notifs.load(Ordering::Relaxed);
                let failures = h_failures.load(Ordering::Relaxed);
                let last_notif = h_last_notif.load(Ordering::Relaxed);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                let since_last = now.saturating_sub(last_notif);
                let delta_notifs = notifs - prev_notifs;
                let delta_failures = failures.saturating_sub(prev_failures);

                let status = if since_last > REBOOTSTRAP_TIMEOUT {
                    "\x1b[1;31mSTALLED\x1b[0m"
                } else if delta_failures > 0 {
                    "\x1b[33mDEGRADED\x1b[0m"
                } else {
                    "\x1b[32mOK\x1b[0m"
                };

                println!(
                    "\x1b[90m[HEALTH] {} | recv={} | bcast_failures={} | last_notif={}s ago\x1b[0m",
                    status, delta_notifs, delta_failures, since_last,
                );

                prev_notifs = notifs;
                prev_failures = failures;
            }
        });
    }

    println!(
        "\n  Listening for gossip notifications. This will take a minute. Patience grasshopper...\n"
    );

    // Channel for forwarding NeighborUp to catch-up task (if enabled)
    let neighbor_tx_for_event: Arc<Mutex<Option<tokio::sync::mpsc::Sender<iroh::EndpointId>>>> = if catchup_enabled {
        let catchup_endpoint = endpoint.clone();
        let catchup_peers_file = peers_file.clone();
        let catchup_bootstrap_str = bootstrap_peer_ids_str.clone();
        let catchup_trusted = trusted_publishers.clone();
        let catchup_last = last_notification_time.clone();
        let catchup_db = db.clone();
        let catchup_sse_tx = sse_tx.clone();

        let since = if let Some(ref db_arc) = db {
            let db_lock = db_arc.lock().unwrap();
            db_lock.latest_timestamp()
                .ok()
                .flatten()
                .unwrap_or_else(|| {
                    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 86400
                })
        } else {
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 86400
        };

        let (neighbor_tx, mut neighbor_rx) = tokio::sync::mpsc::channel::<iroh::EndpointId>(1);
        let tx_arc = Arc::new(Mutex::new(Some(neighbor_tx)));

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            run_catchup(
                &catchup_endpoint,
                since,
                &catchup_peers_file,
                &catchup_bootstrap_str,
                &mut neighbor_rx,
                &catchup_trusted,
                &catchup_last,
                &catchup_db,
                &catchup_sse_tx,
            ).await;
        });

        tx_arc
    } else {
        Arc::new(Mutex::new(None))
    };

    let initial_receive_generation = receive_generation.fetch_add(1, Ordering::Relaxed) + 1;
    spawn_receive_task(
        gossip_receiver,
        peers_file.clone(),
        my_node_id,
        trusted_publishers.clone(),
        trusted_publishers_file.clone(),
        last_notification_time.clone(),
        db.clone(),
        neighbor_tx_for_event.clone(),
        peer_names.clone(),
        sse_tx.clone(),
        notifications_received.clone(),
        broadcast_failures.clone(),
        reconnect_requested.clone(),
        reconnect_notify.clone(),
        receive_generation.clone(),
        initial_receive_generation,
    );

    tokio::signal::ctrl_c().await?;
    println!("\n[info] shutting down...");

    endpoint.close().await;
    println!("[info] goodbye");
    Ok(())
}

//Incoming Iroh gossip event handler
fn handle_event(event: Event, peers_file: &str, my_node_id: &iroh::EndpointId, trusted_publishers: &Arc<RwLock<HashSet<String>>>, trusted_publishers_file: &str, last_notification_time: &Arc<AtomicU64>, db: &Option<Arc<Mutex<archive::Archive>>>, neighbor_tx: &Arc<Mutex<Option<tokio::sync::mpsc::Sender<iroh::EndpointId>>>>, peer_names: &Arc<RwLock<HashMap<String, String>>>, sse_tx: &Option<tokio::sync::broadcast::Sender<String>>, notifications_received: &Arc<AtomicU64>) {
    match event {
        Event::Received(msg) => {
            let raw = &msg.content[..];
            // Try PeerAnnounce first
            if let Ok(announce) = serde_json::from_slice::<PeerAnnounce>(raw) {
                if announce.msg_type == "peer_announce" {
                    if let Some(ref name) = announce.friendly_name {
                        let display_name = sanitize_friendly_name(name);
                        println!("\x1b[33m[ANNOUNCE] PeerAnnounce from \"{}\" ({}) v{}\x1b[0m", display_name, announce.node_id, announce.version);
                    } else {
                        println!("\x1b[33m[ANNOUNCE] PeerAnnounce from {} v{}\x1b[0m", announce.node_id, announce.version);
                    }
                    if version_is_newer(&announce.version, env!("CARGO_PKG_VERSION")) {
                        println!("\x1b[1;41;37m *** A newer version (v{}) is available! You are running v{}. Please upgrade. *** \x1b[0m", announce.version, env!("CARGO_PKG_VERSION"));
                    }
                    if let Ok(node_id) = announce.node_id.parse() {
                        save_peer_if_new(peers_file, &node_id, my_node_id);
                    }
                    if let Some(ref name) = announce.friendly_name {
                        let sanitized = sanitize_friendly_name(name);
                        if !sanitized.is_empty() {
                            let key = announce.node_id.clone();
                            let mut names = peer_names.write().unwrap();
                            let is_new = names.get(&key).map_or(true, |old| *old != sanitized);
                            names.insert(key, sanitized);
                            if is_new {
                                print_peer_table(&names);
                            }
                        }
                    }
                } else if announce.msg_type == "peer_endorse" {
                    // Verify signature
                    match announce.verify_endorse_signature() {
                        Ok(true) => {}
                        Ok(false) => {
                            eprintln!("\x1b[35m[WARN] PeerEndorse without signature, ignoring\x1b[0m");
                            return;
                        }
                        Err(e) => {
                            eprintln!("\x1b[35m[WARN] PeerEndorse bad signature: {e}\x1b[0m");
                            return;
                        }
                    }

                    let sender = match &announce.sender {
                        Some(s) => s.clone(),
                        None => return,
                    };

                    // Check if the endorser is trusted
                    let is_trusted = {
                        let tp = trusted_publishers.read().unwrap();
                        tp.contains(&sender)
                    };

                    if !is_trusted {
                        println!("\x1b[33m[ENDORSE] PeerEndorse from untrusted sender {}, ignoring\x1b[0m", &sender[..8.min(sender.len())]);
                        return;
                    }

                    if let Some(ref name) = announce.friendly_name {
                        let sanitized = sanitize_friendly_name(name);
                        if !sanitized.is_empty() {
                            let mut names = peer_names.write().unwrap();
                            // Store under both sender pubkey (for notification display)
                            // and node_id (for peer table consistency)
                            let mut changed = false;
                            if names.get(&sender).map_or(true, |old| *old != sanitized) {
                                names.insert(sender.clone(), sanitized.clone());
                                changed = true;
                            }
                            let node_key = announce.node_id.clone();
                            if names.get(&node_key).map_or(true, |old| *old != sanitized) {
                                names.insert(node_key, sanitized);
                                changed = true;
                            }
                            if changed {
                                print_peer_table(&names);
                            }
                        }
                    }

                    // Add endorsed keys
                    if let Some(ref node_list) = announce.node_list {
                        let mut added = 0;
                        {
                            let mut tp = trusted_publishers.write().unwrap();
                            for key in node_list {
                                if tp.insert(key.clone()) {
                                    added += 1;
                                    println!("\x1b[32m[ENDORSE] Added trusted publisher {} (endorsed by {})\x1b[0m", &key[..8.min(key.len())], &sender[..8.min(sender.len())]);
                                }
                            }
                        }
                        if added > 0 {
                            let tp = trusted_publishers.read().unwrap();
                            save_trusted_publishers(trusted_publishers_file, &tp);
                            println!("\x1b[32m[ENDORSE] {} new keys added, {} total trusted publishers\x1b[0m", added, tp.len());
                        }
                    }
                }
            } else {
                // Fall back to GossipNotification
                match serde_json::from_slice::<GossipNotification>(raw) {
                    Ok(notif) => {
                        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                        last_notification_time.store(now, Ordering::Relaxed);
                        notifications_received.fetch_add(1, Ordering::Relaxed);
                        let tp = trusted_publishers.read().unwrap();
                        if tp.is_empty() || tp.contains(&notif.sender) {
                            let sender_display = {
                                let names = peer_names.read().unwrap();
                                match names.get(&notif.sender) {
                                    Some(name) => format!("\"{}\" ({})", name, &notif.sender[..8.min(notif.sender.len())]),
                                    None => notif.sender[..8.min(notif.sender.len())].to_string(),
                                }
                            };
                            let json = print_notification(&notif, &sender_display);
                            if let Some(ref tx) = sse_tx {
                                let _ = tx.send(json);
                            }
                            if let Some(ref db_arc) = db {
                                let db_lock = db_arc.lock().unwrap();
                                match db_lock.store(
                                    raw,
                                    &notif.sender,
                                    &notif.medium,
                                    &notif.reason,
                                    notif.timestamp,
                                    notif.iris.len(),
                                ) {
                                    Ok(true) => println!("\x1b[36m[ARCHIVE] Stored (new)\x1b[0m"),
                                    Ok(false) => {}
                                    Err(e) => eprintln!("\x1b[35m[WARN] Archive error: {}\x1b[0m", e),
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "\x1b[35m[WARN] failed to parse notification: {e}\n  raw: {}\x1b[0m",
                            String::from_utf8_lossy(raw)
                        );
                    }
                }
            }
        }
        Event::NeighborUp(node_id) => {
            let node_str = node_id.to_string();
            let display = {
                let names = peer_names.read().unwrap();
                match names.get(&node_str) {
                    Some(name) => format!("\"{}\" ({})", name, node_id),
                    None => node_str,
                }
            };
            println!("\x1b[32m[EVENT] NeighborUp: {display}\x1b[0m");
            save_peer_if_new(peers_file, &node_id, my_node_id);
            // Forward to catch-up task if waiting (std::sync::Mutex is fine here:
            // lock held briefly, no .await while held, called from sync function)
            let mut tx_guard = neighbor_tx.lock().unwrap();
            if let Some(tx) = tx_guard.as_ref() {
                let _ = tx.try_send(node_id);
                *tx_guard = None;
            }
        }
        Event::NeighborDown(node_id) => {
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
        Event::Lagged => {
            eprintln!("\x1b[35m[WARN] lagged — missed some messages\x1b[0m");
        }
    }
}

//Pretty print messages
fn print_notification(notif: &GossipNotification, sender_display: &str) -> String {
    let sig_status = match notif.verify_signature() {
        Ok(true) => "VALID",
        Ok(false) => "UNSIGNED",
        Err(_) => "INVALID",
    };

    let mut json = serde_json::to_value(notif).unwrap_or_default();
    if let Some(obj) = json.as_object_mut() {
        obj.insert("sig_status".to_string(), serde_json::Value::String(sig_status.to_string()));
        obj.insert("sender_name".to_string(), serde_json::Value::String(sender_display.to_string()));
    }
    let json_string = serde_json::to_string(&json).unwrap_or_default();
    println!("PODPING: [{}]", json_string);
    json_string
}

// Sanitize a friendly name received from a remote peer: strip control chars, limit to 64 chars
fn sanitize_friendly_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect::<String>()
        .trim()
        .to_string()
}

fn print_peer_table(names: &HashMap<String, String>) {
    println!("\x1b[36m--- Known peers ({}) ---\x1b[0m", names.len());
    for (key, name) in names {
        let short_key = &key[..16.min(key.len())];
        println!("\x1b[36m  \"{}\"  -> {}...\x1b[0m", name, short_key);
    }
    println!("\x1b[36m---\x1b[0m");
}

// Compare two semver-style version strings (e.g. "0.1.4").
// Returns true if `remote` is strictly newer than `local`.
fn version_is_newer(remote: &str, local: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.').filter_map(|p| p.parse().ok()).collect()
    };
    let r = parse(remote);
    let l = parse(local);
    for i in 0..r.len().max(l.len()) {
        let rv = r.get(i).copied().unwrap_or(0);
        let lv = l.get(i).copied().unwrap_or(0);
        if rv > lv { return true; }
        if rv < lv { return false; }
    }
    false
}

//Load trusted sender public keys from the trusted sender file
//Returns empty HashSet if the file doesn't exist or is empty
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

//Save trusted publishers to file (atomic overwrite)
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

//Load known peers from a text file
//Returns an empty vec if the file doesn't exist or is empty
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

//Save a peer's NodeId to the known-peers file if it's not already in there
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

    //Evict oldest entries if over the max cap
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
        println!("[info] Saved new peer to {}: {}", path, node_str);
    }
}

//Load a persistent iroh node key from `path` or generate a new one and save it
//So we can be the same node identity every time
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
        //Fall back to raw 32-byte format
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

/// Spawn an async task that processes incoming gossip events. Called on startup
/// and again after reconnection.
fn spawn_receive_task(
    receiver: DttGossipReceiver,
    peers_file: String,
    my_node_id: iroh::EndpointId,
    trusted_publishers: Arc<RwLock<HashSet<String>>>,
    trusted_publishers_file: String,
    last_notification_time: Arc<AtomicU64>,
    db: Option<Arc<Mutex<archive::Archive>>>,
    neighbor_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<iroh::EndpointId>>>>,
    peer_names: Arc<RwLock<HashMap<String, String>>>,
    sse_tx: Option<tokio::sync::broadcast::Sender<String>>,
    notifications_received: Arc<AtomicU64>,
    reconnect_failures: Arc<AtomicU64>,
    reconnect_requested: Arc<AtomicBool>,
    reconnect_notify: Arc<Notify>,
    receive_generation_counter: Arc<AtomicU64>,
    receive_generation: u64,
) {
    tokio::spawn(async move {
        while let Some(event) = receiver.next().await {
            match event {
                Ok(event) => handle_event(
                    event,
                    &peers_file,
                    &my_node_id,
                    &trusted_publishers,
                    &trusted_publishers_file,
                    &last_notification_time,
                    &db,
                    &neighbor_tx,
                    &peer_names,
                    &sse_tx,
                    &notifications_received,
                ),
                Err(e) => {
                    eprintln!("\x1b[35m[WARN] Gossip receiver error: {}\x1b[0m", e);
                    break;
                }
            }
        }
        println!("\x1b[33m[RECV] Gossip receive task ended.\x1b[0m");
        if receive_generation_counter.load(Ordering::Relaxed) == receive_generation {
            eprintln!("\x1b[1;31m[RECONNECT] Active receive task exited — reconnecting gossip topic...\x1b[0m");
            reconnect_failures.store(RECONNECT_AFTER_FAILURES, Ordering::Relaxed);
            reconnect_requested.store(true, Ordering::Relaxed);
            reconnect_notify.notify_one();
        }
    });
}
