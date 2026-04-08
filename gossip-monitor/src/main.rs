mod swarm;
mod web;

use crate::swarm::PeerAnnounce;
use crate::swarm::PeerRegistry;
use distributed_topic_tracker::{
    AutoDiscoveryGossip, GossipReceiver as DttGossipReceiver, RecordPublisher,
    TopicId as DttTopicId,
};
use ed25519_dalek::SigningKey;
use iroh::protocol::Router;
use iroh::SecretKey;
use iroh_gossip::api::Event;
use iroh_gossip::net::Gossip;
use std::env;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

const TOPIC_STRING: &str = "gossipping/v1/all";
const DEFAULT_NODE_KEY_FILE: &str = "gossip_monitor_node.key";
const DEFAULT_KNOWN_PEERS_FILE: &str = "gossip_monitor_known_peers.txt";
const MAX_KNOWN_PEERS: usize = 15;
const DEFAULT_DHT_SECRET: &str = "podping_gossip_default_secret";
const DEFAULT_WEB_BIND_ADDR: &str = "0.0.0.0:8090";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Non-blocking tracing to stderr
    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    println!("gossip-monitor v{}", env!("CARGO_PKG_VERSION"));

    // Load config from env vars
    let node_key_file =
        env::var("IROH_NODE_KEY_FILE").unwrap_or_else(|_| DEFAULT_NODE_KEY_FILE.to_string());
    let peers_file =
        env::var("KNOWN_PEERS_FILE").unwrap_or_else(|_| DEFAULT_KNOWN_PEERS_FILE.to_string());
    let dht_initial_secret =
        env::var("DHT_INITIAL_SECRET").unwrap_or_else(|_| DEFAULT_DHT_SECRET.to_string());
    let web_bind_addr: SocketAddr = env::var("WEB_BIND_ADDR")
        .unwrap_or_else(|_| DEFAULT_WEB_BIND_ADDR.to_string())
        .parse()
        .expect("Invalid WEB_BIND_ADDR");

    // Load or create node key
    let node_key = load_or_create_node_key(&node_key_file)?;
    let node_key_bytes = node_key.to_bytes();

    // Create iroh Endpoint
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(node_key)
        .bind()
        .await?;
    let my_node_id = endpoint.id();
    println!("  Node ID: {}", my_node_id);

    // Create Gossip
    let gossip = Gossip::builder()
        .max_message_size(65536)
        .spawn(endpoint.clone());

    // Create Router
    let _router = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();

    // Create DTT topic and publisher
    let dht_signing_key = SigningKey::from_bytes(&node_key_bytes);
    let dtt_topic_id = DttTopicId::new(TOPIC_STRING.to_string());
    println!("  Topic ID: {}", hex::encode(dtt_topic_id.hash()));

    let record_publisher = RecordPublisher::new(
        dtt_topic_id,
        dht_signing_key.verifying_key(),
        dht_signing_key,
        None,
        dht_initial_secret.into_bytes(),
    );

    // Subscribe to topic
    let topic = gossip
        .subscribe_and_join_with_auto_discovery_no_wait(record_publisher)
        .await?;
    let (sender, receiver) = topic.split().await?;
    println!("  Joined gossip topic with DHT auto-discovery.");

    // Join bootstrap peers from env
    let bootstrap_peers: Vec<iroh::EndpointId> = env::var("BOOTSTRAP_PEER_IDS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    // Join known peers from file
    let mut all_peers: Vec<iroh::EndpointId> = load_known_peers(&peers_file);
    for p in &bootstrap_peers {
        if !all_peers.contains(p) {
            all_peers.push(*p);
        }
    }

    if !all_peers.is_empty() {
        println!("  Joining {} peers...", all_peers.len());
        if let Err(e) = sender.join_peers_direct(all_peers, None).await {
            eprintln!("  Warning: failed to join peers: {}", e);
        }
    }

    // Create registry and start web server
    let registry = Arc::new(PeerRegistry::new());
    let sse_tx = web::start_web_server(web_bind_addr, registry.clone());
    println!("  Web server listening on {}", web_bind_addr);

    // Periodic task: broadcast SwarmSnapshot via SSE every 5 seconds
    let sse_registry = registry.clone();
    let sse_sender = sse_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let snapshot = sse_registry.snapshot();
            if let Ok(json) = serde_json::to_string(&snapshot) {
                let _ = sse_sender.send(json);
            }
        }
    });

    // Periodic task: prune stale peers every 300 seconds
    let prune_registry = registry.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let pruned = prune_registry.prune_stale();
            if pruned > 0 {
                println!("  Pruned {} stale peers", pruned);
            }
        }
    });

    // Receive loop
    println!("  Listening for gossip events... (Ctrl+C to stop)");
    receive_loop(receiver, registry, &peers_file, my_node_id).await;

    // Shutdown
    println!("  Shutting down...");
    let shutdown = endpoint.close();
    if tokio::time::timeout(std::time::Duration::from_secs(5), shutdown)
        .await
        .is_err()
    {
        eprintln!("  Warning: endpoint close timed out after 5s");
    }

    std::process::exit(0);
}

async fn receive_loop(
    receiver: DttGossipReceiver,
    registry: Arc<PeerRegistry>,
    peers_file: &str,
    my_node_id: iroh::EndpointId,
) {
    loop {
        tokio::select! {
            event = receiver.next() => {
                let event = match event {
                    Some(event) => event,
                    None => {
                        eprintln!("  Gossip receiver stream ended.");
                        break;
                    }
                };
                match event {
                    Ok(Event::Received(msg)) => {
                        if let Ok(announce) = serde_json::from_slice::<PeerAnnounce>(&msg.content) {
                            if announce.msg_type == "peer_announce" {
                                registry.update(&announce);
                                save_peer_if_new(
                                    peers_file,
                                    &announce.node_id,
                                    &my_node_id.to_string(),
                                );
                            }
                        }
                    }
                    Ok(Event::NeighborUp(node_id)) => {
                        println!("  [+] Neighbor up: {}", node_id);
                    }
                    Ok(Event::NeighborDown(node_id)) => {
                        println!("  [-] Neighbor down: {}", node_id);
                    }
                    Ok(Event::Lagged) => {
                        eprintln!("  [WARN] Gossip receiver lagged");
                    }
                    Err(e) => {
                        eprintln!("  [WARN] Gossip receive error: {}", e);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("  Ctrl+C received.");
                break;
            }
        }
    }
}

fn load_or_create_node_key(path: &str) -> anyhow::Result<SecretKey> {
    if Path::new(path).exists() {
        let raw = fs::read(path)?;
        // Try string-based parse first (backward compat)
        if let Ok(s) = std::str::from_utf8(&raw) {
            if let Ok(key) = s.trim().parse::<SecretKey>() {
                println!("  Loaded node key from {}", path);
                return Ok(key);
            }
        }
        // Fall back to raw 32-byte format
        let key_bytes: [u8; 32] = raw
            .try_into()
            .map_err(|_| anyhow::anyhow!("key file {} has invalid length", path))?;
        println!("  Loaded node key from {}", path);
        Ok(SecretKey::from_bytes(&key_bytes))
    } else {
        let key = SecretKey::generate(&mut rand::rng());
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(path, key.to_bytes())?;
        println!("  Generated new node key -> {}", path);
        Ok(key)
    }
}

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

fn save_peer_if_new(path: &str, node_id: &str, my_node_id: &str) {
    if node_id == my_node_id {
        return;
    }
    let mut peers: Vec<String> = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if peers.iter().any(|l| l == node_id) {
        return;
    }

    peers.push(node_id.to_string());

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
    }
}
