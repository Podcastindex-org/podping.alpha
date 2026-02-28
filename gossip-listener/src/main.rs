use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use std::io::Write;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use iroh::protocol::Router;
use iroh::SecretKey;
use iroh_gossip::api::Event;
use iroh_gossip::net::Gossip;
use serde::{Deserialize, Serialize};
use distributed_topic_tracker::{AutoDiscoveryGossip, RecordPublisher, TopicId as DttTopicId};

const TOPIC_STRING: &str = "gossipping/v1/all";
const DEFAULT_NODE_KEY_FILE: &str = "gossip_listener_node.key";
const DEFAULT_KNOWN_PEERS_FILE: &str = "gossip_listener_known_peers.txt";
const MAX_KNOWN_PEERS: usize = 15;
const DEFAULT_DHT_SECRET: &str = "podping_gossip_default_secret";
const DEFAULT_TRUSTED_PUBLISHERS_FILE: &str = "trusted_publishers.txt";
const DEFAULT_PEER_ENDORSE_INTERVAL: u64 = 45;

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

//Main ---------------------------------------------------------------------------------------------
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("--- Podping Gossip Listener ---\n");

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

    let trusted_publishers = Arc::new(RwLock::new(load_trusted_publishers(&trusted_publishers_file)));

    println!("  Topic: \"{}\"", TOPIC_STRING);
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

    //Set up Iroh context
    let node_key = load_or_create_node_key(&node_key_file)?;
    let node_key_bytes = node_key.to_bytes();
    let endpoint = iroh::Endpoint::builder()
        .secret_key(node_key)
        .bind()
        .await?;

    // Create ed25519 signing key for peer_endorse messages (derived from node key)
    let signing_key = SigningKey::from_bytes(&node_key_bytes);
    let pubkey_hex = hex::encode(signing_key.verifying_key().to_bytes());

    //Self assign an Iroh node id
    let my_node_id = endpoint.id();
    println!("  Iroh Node ID: {}", my_node_id);
    println!("  Sender pubkey: {}", pubkey_hex);

    //Launch Iroh modules
    let gossip = Gossip::builder()
        .max_message_size(65536)
        .spawn(endpoint.clone());
    let _router = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();

    // Bootstrap this node over DHT
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
        if let Err(e) = gossip_sender.join_peers(bootstrap_peers, None).await {
            eprintln!("  Warning: failed to join bootstrap peers: {}", e);
        }
    }

    //Periodically announce ourselves to the topic for the bootstrapping benefit of others
    let peer_announce_interval: u64 = env::var("PEER_ANNOUNCE_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    println!("  Announce interval: {}s", peer_announce_interval);
    println!("  Endorse interval: {}s", peer_endorse_interval);

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

    // --- Periodic PeerEndorse broadcast task ---
    if peer_endorse_interval > 0 {
        let endorse_sender = gossip_sender.clone();
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
                        if let Err(e) = endorse_sender.broadcast(payload).await {
                            eprintln!("[error] Failed to broadcast PeerEndorse: {}", e);
                        } else {
                            println!("[info] Broadcast PeerEndorse ({} keys)", endorse.node_list.as_ref().map_or(0, |l| l.len()));
                        }
                    }
                    Err(e) => eprintln!("[error] Failed to serialize PeerEndorse: {}", e),
                }
            }
        });
    }

    println!(
        "\n  Listening for gossip notifications. This will take a minute. Patience grasshopper...\n"
    );

    // Main receive loop.  CTRL+C to close
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            item = gossip_receiver.next() => {
                match item {
                    Some(Ok(event)) => handle_event(event, &peers_file, &my_node_id, &trusted_publishers, &trusted_publishers_file),
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

//Incoming Iroh gossip event handler
fn handle_event(event: Event, peers_file: &str, my_node_id: &iroh::EndpointId, trusted_publishers: &Arc<RwLock<HashSet<String>>>, trusted_publishers_file: &str) {
    match event {
        Event::Received(msg) => {
            let raw = &msg.content[..];
            // Try PeerAnnounce first
            if let Ok(announce) = serde_json::from_slice::<PeerAnnounce>(raw) {
                if announce.msg_type == "peer_announce" {
                    println!("\x1b[33m[ANNOUNCE] PeerAnnounce from {} v{}\x1b[0m", announce.node_id, announce.version);
                    if let Ok(node_id) = announce.node_id.parse() {
                        save_peer_if_new(peers_file, &node_id, my_node_id);
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
                        let tp = trusted_publishers.read().unwrap();
                        if tp.is_empty() || tp.contains(&notif.sender) {
                            print_notification(&notif);
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
            println!("\x1b[32m[EVENT] NeighborUp: {node_id}\x1b[0m");
            save_peer_if_new(peers_file, &node_id, my_node_id);
        }
        Event::NeighborDown(node_id) => {
            println!("\x1b[31m[EVENT] NeighborDown: {node_id}\x1b[0m");
        }
        Event::Lagged => {
            eprintln!("\x1b[35m[WARN] lagged — missed some messages\x1b[0m");
        }
    }
}

//Pretty print messages
fn print_notification(notif: &GossipNotification) {
    let sig_status = match notif.verify_signature() {
        Ok(true) => "VALID",
        Ok(false) => "UNSIGNED",
        Err(_) => "INVALID",
    };

    let mut json = serde_json::to_value(notif).unwrap_or_default();
    if let Some(obj) = json.as_object_mut() {
        obj.insert("sig_status".to_string(), serde_json::Value::String(sig_status.to_string()));
    }
    println!("PODPING: [{}]", serde_json::to_string(&json).unwrap_or_default());
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