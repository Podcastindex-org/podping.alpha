use std::env;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures_lite::StreamExt;
use iroh::protocol::Router;
use iroh_gossip::net::{Event, Gossip, GossipEvent, GOSSIP_ALPN};
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TOPIC_STRING: &str = "gossipping/v1/all";

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

    // --- Derive topic ID (must match gossip-writer) ---
    let mut hasher = Sha256::new();
    hasher.update(TOPIC_STRING.as_bytes());
    let topic_hash: [u8; 32] = hasher.finalize().into();
    let topic_id = TopicId::from_bytes(topic_hash);
    println!("  Topic: \"{}\"", TOPIC_STRING);
    println!("  Topic ID: {}", hex::encode(topic_hash));

    // --- Set up Iroh endpoint and gossip ---
    let endpoint = iroh::Endpoint::builder().discovery_n0().bind().await?;

    let my_node_id = endpoint.node_id();
    println!("  Iroh Node ID: {}", my_node_id);

    let gossip = Gossip::builder()
        .max_message_size(65536)
        .spawn(endpoint.clone())
        .await?;

    let router = Router::builder(endpoint.clone())
        .accept(GOSSIP_ALPN, gossip.clone())
        .spawn()
        .await?;

    // --- Parse bootstrap peers ---
    let bootstrap_peers: Vec<iroh::NodeId> = bootstrap_peer_ids_str
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    if bootstrap_peers.is_empty() {
        println!("  No bootstrap peers configured; waiting for incoming connections.");
    } else {
        println!("  Bootstrap peers: {:?}", bootstrap_peers);
    }

    // --- Subscribe to the topic ---
    let (sink, mut stream) = gossip.subscribe(topic_id, bootstrap_peers)?.split();
    drop(sink); // we only receive

    println!("\n  Listening for gossip notifications...\n");

    // --- Receive loop with Ctrl+C handling ---
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            item = stream.next() => {
                match item {
                    Some(Ok(event)) => handle_event(event),
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

    router.shutdown().await?;
    println!("[info] goodbye");
    Ok(())
}

fn handle_event(event: Event) {
    match event {
        Event::Gossip(GossipEvent::Received(msg)) => {
            let raw = &msg.content[..];
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
        Event::Gossip(GossipEvent::Joined(peers)) => {
            println!("[event] Joined topic with {} peer(s)", peers.len());
        }
        Event::Gossip(GossipEvent::NeighborUp(node_id)) => {
            println!("[event] NeighborUp: {node_id}");
        }
        Event::Gossip(GossipEvent::NeighborDown(node_id)) => {
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
