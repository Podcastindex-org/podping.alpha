# Gossipping: A Podping Alternative Using Iroh Gossip

## Overview

**Gossipping** is a decentralized podcast feed notification system that replaces Podping's Hive blockchain layer with [Iroh's gossip protocol](https://github.com/n0-computer/iroh-gossip). When a podcast hosting company publishes or updates an episode, it broadcasts a small notification to a peer-to-peer gossip swarm. Podcast apps and aggregators listening on that swarm receive the notification within seconds and can fetch the updated feed — no polling required.

### Why Replace the Blockchain?

Podping works well, but the Hive blockchain introduces some friction:

- **Write costs**: Every notification is a blockchain transaction (even if cheap).
- **Account management**: Publishers need Hive accounts with staked tokens.
- **Latency**: Hive blocks are produced every ~3 seconds; gossip can propagate in milliseconds.
- **Dependency**: The system's availability is coupled to Hive's network health.
- **Complexity**: Running a Hive node or using the API adds operational overhead.

Iroh gossip removes all of these while preserving the core value proposition: a single firehose that any publisher can write to and any consumer can read from.

---

## Architecture

### Participants

There are three roles in the system (a single node can fulfill multiple roles):

| Role | Description | Example |
|------|-------------|---------|
| **Publisher** | Sends feed update notifications (via HTTP API or gossip) | Podcast hosting company (Buzzsprout, Podbean, self-hosters) |
| **Consumer** | Listens for notifications and acts on them | Podcast apps (AntennaPod, Fountain), aggregators (Podcast Index) |
| **Relay** | Long-lived node that stabilizes the swarm | Community-operated infrastructure nodes |

Every node — regardless of role — exposes an **HTTP REST API** for accepting podpings and querying the archive. This means any node can serve as an ingestion point for publishers who just want to send a simple HTTP request, exactly like podping.cloud works today.

### Network Topology

```
                    ┌──────────┐
                    │  Relay A │
                    └────┬─────┘
                         │
        ┌────────────────┼────────────────┐
        │                │                │
  ┌─────┴─────┐   ┌─────┴─────┐   ┌──────┴────┐
  │ Publisher  │   │ Consumer  │   │  Relay B  │
  │ (Host Co.) │   │ (App)     │   │           │
  └───────────┘   └───────────┘   └─────┬─────┘
                                        │
                                  ┌─────┴─────┐
                                  │ Consumer  │
                                  │ (Index)   │
                                  └───────────┘
```

All participants join a single gossip **topic** identified by a well-known `TopicId`. Iroh gossip uses [HyParView](https://asc.di.fct.unl.pt/~jleitao/pdf/dsn07-leitao.pdf) for peer membership and [PlumTree](https://asc.di.fct.unl.pt/~jleitao/pdf/srds07-leitao.pdf) for message dissemination, giving the network:

- **Resilient membership**: Nodes can join/leave freely; the overlay self-heals.
- **Efficient broadcast**: Messages travel along a spanning tree with lazy repair via gossip.
- **Bounded fan-out**: Each node maintains a small, fixed-size set of neighbors.

### Bootstrap & Discovery

New nodes need at least one existing peer to join the swarm. The system uses a layered discovery approach:

1. **Well-known relay list**: A small set of community-operated relays published at a known URL (e.g., `https://gossipping.org/relays.json`) and/or hardcoded as defaults.
2. **Iroh DNS discovery**: Relays register their `EndpointId` with Iroh's DNS discovery service (`dns.iroh.link`), allowing lookup by a human-readable name.
3. **DHT discovery**: Iroh's built-in DHT discovery (`DhtDiscovery`) enables finding peers without centralized infrastructure.
4. **Manual peering**: Operators can exchange `NodeTicket` strings out-of-band.

---

## Message Format

Each gossip message is a **notification** — a small, self-contained JSON payload. The format is intentionally compatible with Podping's schema to ease migration:

```json
{
  "version": "1.0",
  "sender": "<ed25519 public key of the publisher>",
  "timestamp": 1708531200,
  "medium": "podcast",
  "reason": "update",
  "iris": [
    "https://feeds.example.com/show1.rss",
    "https://feeds.example.com/show2.rss"
  ],
  "signature": "<ed25519 signature over the canonical message>"
}
```

### Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `version` | string | ✅ | Schema version (`"1.0"`) |
| `sender` | string | ✅ | Hex-encoded ed25519 public key of the publishing node |
| `timestamp` | integer | ✅ | Unix timestamp (seconds) when the notification was created |
| `medium` | string | ✅ | Content type: `podcast`, `music`, `video`, `film`, `audiobook`, `newsletter`, `blog`, `publisher`, `course` |
| `reason` | string | ✅ | Why: `update` (new/changed episode), `live` (live stream started), `liveEnd` (live stream ended) |
| `iris` | string[] | ✅ | List of feed IRIs (URLs, IPFS CIDs, etc.) that were updated. Max 100 per message. |
| `signature` | string | ✅ | Hex-encoded ed25519 signature of the canonical form (see below) |

### Canonical Form & Signing

To produce the signature:

1. Serialize the message **without** the `signature` field.
2. Sort all JSON keys alphabetically (recursive).
3. Serialize to compact JSON (no whitespace).
4. Sign the resulting bytes with the sender's ed25519 private key.

This allows any consumer to verify that a message genuinely came from the claimed sender without requiring a blockchain or certificate authority.

### Size Constraints

- Maximum serialized message size: **8 KB** (fits comfortably in a single gossip message).
- Maximum IRIs per message: **100** (batch large updates across multiple messages).
- Consumers SHOULD ignore messages larger than 8 KB.

---

## Identity & Trust

### Publisher Identity

Each publisher is identified by its **Iroh endpoint's ed25519 public key**. This is the same key used for Iroh's QUIC connections, so it comes for free — no separate key management needed.

### Trust Model

The system uses a **publisher registry** rather than an authorization gate. There is no permissioned access to send gossip messages — anyone can broadcast. Instead, trust is established through a public registry:

```
┌─────────────────────────────────────────────────┐
│              Publisher Registry                  │
│  (hosted at gossipping.org or self-hosted)       │
│                                                  │
│  {                                               │
│    "publishers": [                               │
│      {                                           │
│        "name": "Buzzsprout",                     │
│        "public_key": "abc123...",                 │
│        "website": "https://buzzsprout.com",      │
│        "feed_prefixes": ["https://feeds.buzz.."] │
│      }                                           │
│    ]                                             │
│  }                                               │
└─────────────────────────────────────────────────┘
```

**How trust works:**

1. **Open broadcast**: Any node can send a notification. The gossip layer doesn't filter.
2. **Signature verification**: Consumers verify the ed25519 signature on each message.
3. **Registry lookup**: Consumers check whether the sender's public key is in the registry of known publishers.
4. **Consumer discretion**: Each consumer decides its own trust policy:
   - **Strict**: Only process messages from registered publishers.
   - **Open**: Process all well-formed, validly-signed messages.
   - **Mixed**: Trust registered publishers; rate-limit or queue unregistered ones.

This approach preserves the open nature of the system while giving consumers the tools to filter spam.

### Spam Mitigation

Without blockchain transaction costs as a natural spam deterrent, the system uses:

- **Signature requirement**: Every message must be signed; unsigned messages are dropped.
- **Rate limiting**: Consumers can rate-limit messages per public key (e.g., max 60 messages/minute per sender).
- **Deduplication**: Messages are deduplicated by `(sender, timestamp, hash(iris))`.
- **Registry filtering**: Consumers who only care about known publishers can drop everything else.
- **Gossip protocol properties**: Iroh's PlumTree algorithm naturally limits message amplification.

---

## Topic Design

### Single Global Topic (Recommended for v1)

The simplest approach: all publishers and consumers join one topic.

```rust
// Well-known topic ID: SHA-256("gossipping/v1/all")
let topic_id = TopicId::from_bytes(
    sha256(b"gossipping/v1/all")
);
```

This mirrors Podping's single-firehose model and is appropriate for the current scale of the podcast ecosystem (a few thousand publishers, a few hundred consumers).

### Future: Scoped Topics

As the network grows, messages can be split across topics by medium:

```rust
// Per-medium topics
let podcast_topic = TopicId::from_bytes(sha256(b"gossipping/v1/podcast"));
let music_topic   = TopicId::from_bytes(sha256(b"gossipping/v1/music"));
let video_topic   = TopicId::from_bytes(sha256(b"gossipping/v1/video"));
```

Consumers subscribe only to the topics they care about. Publishers send to the appropriate topic. Relays join all topics.

---

## Node Implementation

### Minimal Publisher (Rust)

A publisher node sends its own notifications via gossip AND accepts podpings from other publishers via HTTP:

```rust
use iroh::{protocol::Router, Endpoint};
use iroh_gossip::{Gossip, TopicId};
use ed25519_dalek::{SigningKey, Signer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Set up iroh endpoint
    let endpoint = Endpoint::bind().await?;
    let gossip = Gossip::builder().spawn(endpoint.clone());
    let signing_key = load_or_generate_key()?;
    let archive = Arc::new(SqliteStorage::open("gossipping.db")?);
    let auth_db = Arc::new(AuthDb::open("auth.db")?);
    let catchup = CatchupProtocol { archive: archive.clone() };

    let _router = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .accept(CATCHUP_ALPN, catchup)
        .spawn();

    // 2. Join the gossipping topic
    let topic = TopicId::from_bytes(sha256(b"gossipping/v1/all"));
    let relays = load_bootstrap_peers();
    let (sender, mut receiver) = gossip
        .subscribe(topic, relays)
        .await?;

    receiver.joined().await?;

    // 3. Start the HTTP server (accepts podpings from other publishers)
    let http_router = build_http_router(
        sender.clone(), archive.clone(), auth_db, signing_key.clone(),
    );
    tokio::spawn(
        axum::serve(TcpListener::bind("0.0.0.0:8080").await?, http_router)
    );

    // 4. Send our own notification directly via gossip
    let notification = build_notification(
        &signing_key,
        "podcast",
        "update",
        vec!["https://feeds.example.com/show.rss"],
    )?;

    let payload = serde_json::to_vec(&notification)?;
    sender.broadcast(payload.into()).await?;

    Ok(())
}
```

### Minimal Consumer (Rust)

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let endpoint = Endpoint::bind().await?;
    let gossip = Gossip::builder().spawn(endpoint.clone());

    // Set up archive storage
    let archive = Arc::new(S3Storage::new(
        "s3://my-bucket/gossipping/v1/",
        s3_credentials(),
    )?);

    // Set up catch-up protocol
    let catchup = CatchupProtocol { archive: archive.clone() };

    let _router = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .accept(CATCHUP_ALPN, catchup)
        .spawn();

    let topic = TopicId::from_bytes(sha256(b"gossipping/v1/all"));
    let relays = load_bootstrap_peers();
    let (_sender, mut receiver) = gossip
        .subscribe(topic, relays)
        .await?;

    receiver.joined().await?;

    // Catch up on missed messages first
    let last_seen = archive.latest_timestamp().await?;
    catch_up_from_peers(&endpoint, &archive, last_seen).await?;

    // Then process live messages
    while let Some(event) = receiver.next().await {
        match event? {
            Event::Received(msg) => {
                if let Ok(notification) = parse_and_verify(&msg.content) {
                    // Archive the message
                    let hash = blake3::hash(&msg.content);
                    archive.put_message(&hash.to_hex(), &msg.content).await?;
                    archive.append_manifest(
                        &hour_key(notification.timestamp),
                        &ManifestEntry::from(&notification, &hash),
                    ).await?;

                    // Process the notification
                    for iri in &notification.iris {
                        println!("Feed updated: {}", iri);
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}
```

### Relay Node

A relay is a long-lived node that keeps the gossip overlay healthy, archives all messages, serves catch-up requests, AND accepts podpings via HTTP:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let endpoint = Endpoint::bind().await?;
    let gossip = Gossip::builder().spawn(endpoint.clone());
    let signing_key = load_or_generate_key()?;

    // Relays use durable storage with longer retention
    let archive = Arc::new(S3Storage::new(
        "s3://relay-bucket/gossipping/v1/",
        s3_credentials(),
    )?);
    let auth_db = Arc::new(AuthDb::open("auth.db")?);
    let catchup = CatchupProtocol { archive: archive.clone() };

    let _router = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .accept(CATCHUP_ALPN, catchup)
        .spawn();

    let topic = TopicId::from_bytes(sha256(b"gossipping/v1/all"));
    let (sender, mut receiver) = gossip
        .subscribe(topic, bootstrap_peers())
        .await?;

    // Start the HTTP server
    let http_router = build_http_router(
        sender.clone(), archive.clone(), auth_db, signing_key,
    );
    tokio::spawn(
        axum::serve(TcpListener::bind("0.0.0.0:8080").await?, http_router)
    );

    // Archive every message that flows through
    tokio::spawn({
        let archive = archive.clone();
        async move {
            while let Some(event) = receiver.next().await {
                if let Ok(Event::Received(msg)) = event {
                    if let Ok(notification) = parse_and_verify(&msg.content) {
                        let hash = blake3::hash(&msg.content);
                        let _ = archive.put_message(&hash.to_hex(), &msg.content).await;
                        let _ = archive.append_manifest(
                            &hour_key(notification.timestamp),
                            &ManifestEntry::from(&notification, &hash),
                        ).await;
                    }
                }
            }
        }
    });

    // Run retention cleanup daily
    tokio::spawn({
        let archive = archive.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(86400)).await;
                let _ = archive.delete_older_than(Duration::from_secs(30 * 86400)).await;
            }
        }
    });

    // Keep running until shutdown
    tokio::signal::ctrl_c().await?;
    Ok(())
}
```

---

## HTTP REST API

Every Gossipping node runs an HTTP server alongside its gossip protocol handler. This serves two purposes: it gives publishers a dead-simple way to submit feed updates (just like podping.cloud), and it gives consumers a way to query the archive without joining the gossip swarm.

### Design Goals

- **Drop-in compatible with podping.cloud**: A publisher currently sending `GET https://podping.cloud/?url=...` can switch to any Gossipping node by changing the hostname. Same query parameters, same Authorization header.
- **Any node is an ingestion point**: Publishers don't need to pick a "special" server. Any relay, consumer, or even another publisher's node can accept podpings over HTTP and inject them into the gossip swarm.
- **Archive queryable**: Consumers who can't or don't want to run the gossip protocol (e.g., serverless functions, browser apps) can poll the archive via REST.

### Authentication

Publishers authenticate using bearer tokens, managed per-node in a local database:

```
Authorization: Bearer <token>
```

Each node operator decides which publishers to issue tokens to. This is identical to how podping.cloud works — operators maintain an `auth.db` with valid tokens. A publisher can register tokens with multiple nodes for redundancy.

The token maps to a publisher identity record:

```json
{
  "token": "abc123...",
  "publisher_name": "Buzzsprout",
  "public_key": "def456...",
  "feed_prefixes": ["https://feeds.buzzsprout.com/"],
  "rate_limit": 60,
  "created_at": "2026-01-01T00:00:00Z"
}
```

The `feed_prefixes` field is optional but enables the node to validate that a publisher is only sending notifications for feeds they own.

### Endpoints

#### Submit a Podping

**Podping.cloud-compatible (GET)**

```
GET /?url=<feed_url>&reason=<reason>&medium=<medium>
Authorization: Bearer <token>
```

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `url` | ✅ | — | The feed IRI that was updated |
| `reason` | ❌ | `update` | `update`, `live`, `liveEnd` |
| `medium` | ❌ | `podcast` | `podcast`, `music`, `video`, `film`, `audiobook`, `newsletter`, `blog`, `publisher`, `course` |

Response:

```
200 OK
```

This is the simplest possible integration. A hosting company adds one HTTP call to their publish flow and they're done.

**Batch (POST)**

For publishers updating many feeds at once (e.g., a hosting platform processing a queue):

```
POST /api/v1/podping
Authorization: Bearer <token>
Content-Type: application/json

{
  "reason": "update",
  "medium": "podcast",
  "iris": [
    "https://feeds.example.com/show1.rss",
    "https://feeds.example.com/show2.rss",
    "https://feeds.example.com/show3.rss"
  ]
}
```

Response:

```json
{
  "status": "accepted",
  "message_hash": "abc123...",
  "iri_count": 3,
  "timestamp": 1708531200
}
```

The batch endpoint accepts up to 100 IRIs per request. If a publisher needs to send more, they make multiple requests.

**What happens on the server side:**

```
Publisher                    Node HTTP Server               Gossip Swarm
    │                              │                              │
    │  GET /?url=...&reason=...    │                              │
    │─────────────────────────────►│                              │
    │                              │                              │
    │                              │  1. Validate auth token      │
    │                              │  2. Validate feed URL format │
    │                              │  3. Check rate limit         │
    │                              │  4. Build notification msg   │
    │                              │  5. Sign with node's key     │
    │                              │  6. Archive locally          │
    │                              │  7. Broadcast to gossip      │
    │                              │─────────────────────────────►│
    │                              │                              │
    │  200 OK                      │                              │
    │◄─────────────────────────────│                              │
    │                              │                              │
```

When a node receives a podping via HTTP, it wraps it into the standard gossip message format, signs it with the node's own ed25519 key, archives it, and broadcasts it to the swarm. The node acts as a **delegate signer** — the gossip message's `sender` field is the node's public key, and the `publisher` field (see updated message format below) identifies the original publisher.

#### Query the Archive

Consumers can query any node's archive over HTTP without joining the gossip swarm.

**Recent notifications:**

```
GET /api/v1/notifications?since=<unix_timestamp>&medium=<medium>&reason=<reason>&limit=<n>
```

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `since` | ❌ | 1 hour ago | Unix timestamp; return notifications after this time |
| `medium` | ❌ | all | Filter by medium |
| `reason` | ❌ | all | Filter by reason |
| `limit` | ❌ | 1000 | Max results (capped at 10000) |

Response:

```json
{
  "notifications": [
    {
      "hash": "abc123...",
      "sender": "def456...",
      "publisher": "Buzzsprout",
      "timestamp": 1708531200,
      "medium": "podcast",
      "reason": "update",
      "iris": ["https://feeds.buzzsprout.com/12345.rss"]
    }
  ],
  "count": 1,
  "has_more": false,
  "oldest_available": 1708444800
}
```

**Server-Sent Events (SSE) firehose:**

For consumers that want real-time push without running gossip:

```
GET /api/v1/firehose?medium=<medium>&reason=<reason>
Accept: text/event-stream
```

```
event: notification
data: {"hash":"abc123...","sender":"def456...","timestamp":1708531200,"medium":"podcast","reason":"update","iris":["https://feeds.example.com/show.rss"]}

event: notification
data: {"hash":"ghi789...","sender":"jkl012...","timestamp":1708531205,"medium":"podcast","reason":"live","iris":["https://feeds.example.com/live.rss"]}
```

This gives browser apps, serverless functions, and lightweight consumers a way to receive real-time notifications with zero infrastructure — just open an HTTP connection to any node.

**Node status:**

```
GET /api/v1/status
```

```json
{
  "node_id": "abc123...",
  "version": "0.1.0",
  "uptime_seconds": 86400,
  "gossip_peers": 12,
  "archive": {
    "oldest_message": 1707926400,
    "newest_message": 1708531200,
    "total_messages": 142857,
    "storage_backend": "s3"
  },
  "http": {
    "authorized_publishers": 5,
    "requests_today": 3200
  }
}
```

### Updated Message Format

To support HTTP-submitted podpings, the gossip message format gains an optional `publisher` field that identifies the original publisher when a node is acting as a delegate:

```json
{
  "version": "1.0",
  "sender": "<ed25519 public key of the node that signed and broadcast>",
  "publisher": "<optional: name/identifier of the HTTP publisher>",
  "timestamp": 1708531200,
  "medium": "podcast",
  "reason": "update",
  "iris": [
    "https://feeds.example.com/show1.rss",
    "https://feeds.example.com/show2.rss"
  ],
  "signature": "<ed25519 signature over the canonical message>"
}
```

When a publisher sends notifications directly via gossip (not through the HTTP API), the `publisher` field is omitted and the `sender` key is their own. When a node accepts a podping via HTTP, it sets `sender` to its own key (since it signs the gossip message) and `publisher` to the authenticated publisher's name from the token database.

Consumers can use either field for trust decisions:
- **`sender`**: The node that vouched for this notification. Is this node in the relay registry?
- **`publisher`**: The original publisher. Is this publisher known to the registry?

### Rate Limiting

Each node enforces rate limits per auth token to prevent abuse:

| Limit | Default | Description |
|-------|---------|-------------|
| Requests per minute | 60 | Per auth token |
| IRIs per request | 100 | Per batch POST |
| IRIs per minute | 2000 | Per auth token, across all requests |

Rate limit headers are returned on every response:

```
X-RateLimit-Limit: 60
X-RateLimit-Remaining: 45
X-RateLimit-Reset: 1708531260
```

When a token exceeds its limit:

```
429 Too Many Requests
Retry-After: 15
```

### Implementation

The HTTP server is embedded in every node using a lightweight async framework (e.g., `axum` in Rust):

```rust
use axum::{Router, routing::{get, post}, extract::*};

fn build_http_router(
    gossip_sender: GossipSender,
    archive: Arc<dyn ArchiveStorage>,
    auth_db: Arc<AuthDb>,
    signing_key: SigningKey,
) -> Router {
    Router::new()
        // Podping.cloud-compatible GET endpoint
        .route("/", get(handle_podping_get))
        // Batch POST endpoint
        .route("/api/v1/podping", post(handle_podping_batch))
        // Archive query
        .route("/api/v1/notifications", get(handle_query_notifications))
        // SSE firehose
        .route("/api/v1/firehose", get(handle_firehose_sse))
        // Node status
        .route("/api/v1/status", get(handle_status))
        .with_state(AppState {
            gossip_sender,
            archive,
            auth_db,
            signing_key,
        })
}

async fn handle_podping_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<PodpingParams>,
) -> Result<StatusCode, AppError> {
    // 1. Authenticate
    let token = extract_bearer_token(&headers)?;
    let publisher = state.auth_db.validate_token(&token).await?;

    // 2. Rate limit
    state.auth_db.check_rate_limit(&token).await?;

    // 3. Validate URL
    validate_feed_url(&params.url)?;
    if let Some(prefixes) = &publisher.feed_prefixes {
        validate_feed_prefix(&params.url, prefixes)?;
    }

    // 4. Build, sign, archive, and broadcast
    let notification = Notification {
        version: "1.0".into(),
        sender: state.signing_key.verifying_key().to_hex(),
        publisher: Some(publisher.name.clone()),
        timestamp: now_unix(),
        medium: params.medium.unwrap_or("podcast".into()),
        reason: params.reason.unwrap_or("update".into()),
        iris: vec![params.url],
        signature: String::new(), // filled by sign()
    };

    let signed = sign_notification(&state.signing_key, notification)?;
    let payload = serde_json::to_vec(&signed)?;

    // Archive
    let hash = blake3::hash(&payload);
    state.archive.put_message(&hash.to_hex(), &payload).await?;
    state.archive.append_manifest(
        &hour_key(signed.timestamp),
        &ManifestEntry::from(&signed, &hash),
    ).await?;

    // Broadcast to gossip swarm
    state.gossip_sender.broadcast(payload.into()).await?;

    Ok(StatusCode::OK)
}
```

### Migration from Podping.cloud

The GET endpoint is intentionally identical to podping.cloud's interface. For existing publishers, migration is:

1. **Get a token** from any Gossipping node operator (or run your own node).
2. **Change the hostname** from `podping.cloud` to the Gossipping node's address.
3. **That's it.** Same `GET /?url=...&reason=...&medium=...` format, same `Authorization: Bearer` header.

During the transition period, publishers can send to both podping.cloud and a Gossipping node simultaneously for redundancy.

---

## Node Lifecycle: First Boot to Fully Caught Up

### Overview

A new node goes through four phases before it's fully operational:

```
┌─────────┐    ┌───────────┐    ┌──────────┐    ┌──────────┐
│ 1. BIND │───►│ 2. DISCO- │───►│ 3. CATCH │───►│ 4. LIVE  │
│         │    │    VER    │    │    UP     │    │          │
│ Generate │    │ Find peers│    │ Fill gaps │    │ Steady   │
│ identity │    │ via DNS,  │    │ from peer │    │ state    │
│ & config │    │ DHT, or   │    │ archives  │    │          │
│          │    │ relay list│    │           │    │          │
└─────────┘    └───────────┘    └──────────┘    └──────────┘
```

### Phase 1: Bind — Identity & Configuration

On first boot, the node generates a persistent ed25519 keypair (its `EndpointId`) and configures its storage backend:

```rust
// Generate or load a persistent identity
let secret_key = match std::fs::read("gossipping.key") {
    Ok(bytes) => SecretKey::from_bytes(&bytes)?,
    Err(_) => {
        let key = SecretKey::generate();
        std::fs::write("gossipping.key", key.to_bytes())?;
        key
    }
};

// Bind the iroh endpoint with discovery enabled
let endpoint = Endpoint::builder()
    .secret_key(secret_key)
    .add_discovery(PkarrResolver::n0_dns())      // DNS discovery
    .add_discovery(DhtDiscovery::builder().build()?) // DHT discovery
    .bind()
    .await?;

// Initialize archive storage
let archive = Arc::new(SqliteStorage::open("gossipping.db")?);
```

At this point the node has an identity and can accept connections, but it doesn't know anyone yet.

### Phase 2: Discover — Find Peers

The node needs at least one existing peer to join the gossip swarm. It tries multiple discovery methods in parallel, using whichever succeeds first:

```
Discovery Methods (tried in parallel)
──────────────────────────────────────

1. Well-Known Relay List (fastest, most reliable)
   GET https://gossipping.org/relays.json
   → Returns EndpointIds of community relays

2. Iroh DNS Discovery
   Resolve known relay names via dns.iroh.link
   → e.g., "relay1.gossipping.org" → EndpointId

3. Iroh DHT Discovery
   Query the DHT for peers advertising the topic
   → Fully decentralized, no servers needed

4. Manual Peer (fallback)
   User provides a NodeTicket string from another operator
   → Out-of-band, but always works
```

```rust
async fn discover_bootstrap_peers() -> Vec<EndpointId> {
    let mut peers = Vec::new();

    // Try all methods concurrently
    let (relay_list, dns_peers, dht_peers) = tokio::join!(
        fetch_relay_list("https://gossipping.org/relays.json"),
        resolve_dns_relays(&["relay1.gossipping.org", "relay2.gossipping.org"]),
        discover_dht_peers(),
    );

    // Merge results, dedup by EndpointId
    if let Ok(relays) = relay_list { peers.extend(relays); }
    if let Ok(dns) = dns_peers { peers.extend(dns); }
    if let Ok(dht) = dht_peers { peers.extend(dht); }

    peers.sort();
    peers.dedup();
    peers
}
```

**The well-known relay list** (`relays.json`) is the recommended bootstrap method. It's a simple JSON file hosted at a known URL:

```json
{
  "version": 1,
  "updated": "2026-02-21T00:00:00Z",
  "relays": [
    {
      "name": "Podcast Index Relay",
      "endpoint_id": "abc123...",
      "operator": "Podcast Index",
      "region": "us-east"
    },
    {
      "name": "Community Relay EU",
      "endpoint_id": "def456...",
      "operator": "podcastaddict.com",
      "region": "eu-west"
    }
  ]
}
```

Nodes SHOULD cache this list locally and refresh it periodically (e.g., daily). If the URL is unreachable, the cached version is used. A few relay EndpointIds MAY also be hardcoded in the binary as a last resort.

### Phase 3: Catch Up — Fill the Gap

Once the node has found at least one peer, it joins the gossip topic and immediately enters a dual-mode where it both receives live messages AND backfills history:

```
                        ┌─────────────────────────────┐
                        │        New Node              │
                        │                              │
                   ┌────┴────┐                ┌───────┴───────┐
                   │  LIVE   │                │   CATCH-UP    │
                   │ STREAM  │                │   (parallel)  │
                   │         │                │               │
                   │ Join    │                │ Request       │
                   │ gossip  │                │ manifests     │
                   │ topic,  │                │ from multiple │
                   │ receive │                │ peers, fetch  │
                   │ new msgs│                │ missing msgs  │
                   │ as they │                │               │
                   │ arrive  │                │               │
                   └────┬────┘                └───────┬───────┘
                        │                             │
                        │      Both write to the      │
                        │      same archive with       │
                        │      dedup by content hash   │
                        └──────────┬──────────────────┘
                                   │
                              ┌────▼────┐
                              │ Archive │
                              │  (S3 /  │
                              │ SQLite) │
                              └─────────┘
```

**The catch-up process in detail:**

```rust
async fn catch_up(
    endpoint: &Endpoint,
    archive: &Arc<dyn ArchiveStorage>,
    peers: &[EndpointId],
    since: Option<u64>,  // None for brand new nodes
) -> Result<CatchUpStats> {
    // 1. Determine the starting point
    let since = since.unwrap_or_else(|| {
        // Brand new node: ask for the last 24 hours by default.
        // This is configurable — some nodes may want 7 days.
        now_unix() - 86400
    });

    // 2. Request manifests from multiple peers in parallel.
    //    Using multiple peers gives us redundancy — if one peer
    //    has a gap, another probably doesn't.
    let mut all_entries: HashMap<String, ManifestEntry> = HashMap::new();

    let manifest_futures: Vec<_> = peers.iter()
        .take(3)  // Ask up to 3 peers
        .map(|peer| request_manifests(endpoint, peer, since))
        .collect();

    for result in futures::future::join_all(manifest_futures).await {
        if let Ok(entries) = result {
            for entry in entries {
                // Dedup: same hash = same message, keep one
                all_entries.entry(entry.hash.clone()).or_insert(entry);
            }
        }
    }

    // 3. Diff against our local archive to find what we're missing
    let mut missing: Vec<String> = Vec::new();
    for hash in all_entries.keys() {
        if archive.get_message(hash).await?.is_none() {
            missing.push(hash.clone());
        }
    }

    // 4. Fetch missing messages in batches from available peers.
    //    Distribute across peers for speed.
    let chunks: Vec<_> = missing.chunks(100).collect();
    let mut fetched = 0;

    for (i, chunk) in chunks.iter().enumerate() {
        let peer = &peers[i % peers.len()];  // Round-robin across peers
        match request_messages(endpoint, peer, chunk).await {
            Ok(messages) => {
                for msg in messages {
                    if let Ok(notification) = parse_and_verify(&msg) {
                        let hash = blake3::hash(&msg);
                        archive.put_message(&hash.to_hex(), &msg).await?;
                        archive.append_manifest(
                            &hour_key(notification.timestamp),
                            &ManifestEntry::from(&notification, &hash),
                        ).await?;
                        fetched += 1;
                    }
                }
            }
            Err(e) => {
                // Peer unavailable — remaining hashes will be retried
                // with other peers on next pass
                log::warn!("Catch-up from {peer} failed: {e}");
            }
        }
    }

    Ok(CatchUpStats { requested: missing.len(), fetched })
}
```

**Key design decisions for catch-up:**

- **Brand new nodes default to 24 hours of history.** This is enough to avoid missing any recent notifications. Nodes that need deeper history (e.g., a new aggregator building a full index) can configure a longer catch-up window, but most consumers only care about "what happened while I was offline."
- **Manifests are small.** Each manifest entry is ~100 bytes. A full day of manifests at 50,000 messages/day is ~5 MB — easily transferred in a few seconds.
- **Fetching is parallelized across peers.** Missing messages are split across available peers using round-robin. If one peer fails, those hashes are retried with others.
- **Live and catch-up run concurrently.** The node doesn't wait for catch-up to finish before processing live messages. Both paths write to the same archive with content-hash dedup, so there's no risk of duplicates.
- **Catch-up is progressive, not blocking.** The node is usable immediately upon joining the gossip topic. Catch-up fills in the historical gap in the background.

### Phase 4: Live — Steady State

Once catch-up completes, the node is fully operational. It continues to:

1. Receive live gossip messages and archive them.
2. Serve catch-up requests from other peers.
3. Optionally publish notifications (if it's a publisher).
4. Run periodic retention cleanup.

```rust
// The main loop after catch-up completes
async fn run_steady_state(
    mut receiver: GossipReceiver,
    archive: Arc<dyn ArchiveStorage>,
    config: NodeConfig,
) -> Result<()> {
    loop {
        tokio::select! {
            // Process incoming gossip messages
            Some(event) = receiver.next() => {
                match event? {
                    Event::Received(msg) => {
                        if let Ok(notification) = parse_and_verify(&msg.content) {
                            let hash = blake3::hash(&msg.content);
                            archive.put_message(&hash.to_hex(), &msg.content).await?;
                            archive.append_manifest(
                                &hour_key(notification.timestamp),
                                &ManifestEntry::from(&notification, &hash),
                            ).await?;

                            // Application-specific handling
                            (config.on_notification)(&notification).await?;
                        }
                    }
                    Event::NeighborUp(peer) => {
                        log::info!("Peer joined: {peer}");
                    }
                    Event::NeighborDown(peer) => {
                        log::info!("Peer left: {peer}");
                    }
                    _ => {}
                }
            }
            // Graceful shutdown
            _ = tokio::signal::ctrl_c() => {
                log::info!("Shutting down...");
                break;
            }
        }
    }
    Ok(())
}
```

### Reconnection After Downtime

When an existing node restarts after being offline, the process is the same as phases 2–4, but faster:

- **Phase 2 is near-instant**: The node already has cached peers and its own identity.
- **Phase 3 is minimal**: The `since` timestamp comes from the archive's last known message, so the catch-up window is only the duration of the downtime.
- **Phase 4 resumes normally.**

```rust
// On restart, catch-up is just the gap since last message
let last_seen = archive.latest_timestamp().await?;
catch_up(&endpoint, &archive, &peers, Some(last_seen)).await?;
```

### Edge Cases

**What if no peers are reachable?**
The node retries discovery with exponential backoff (1s, 2s, 4s, ... up to 60s). It logs warnings but doesn't crash. Once any peer becomes available, it joins and catches up.

**What if catch-up peers have shorter retention than the gap?**
The node gets what it can. If peer A only has 7 days but the node was offline for 14 days, it gets 7 days from A and tries other peers for the rest. If no peer has older data, the node accepts the gap and logs it. This is acceptable — the system provides best-effort history, not guaranteed completeness.

**What if a peer sends invalid messages during catch-up?**
Every message is verified (`parse_and_verify`) before archival. Invalid or unsigned messages are silently dropped. A peer that consistently sends bad data can be deprioritized for future catch-up requests.

**What about very large catch-up windows?**
A node requesting months of history (millions of messages) should do so incrementally — fetching one day at a time, with backpressure. The catch-up protocol supports this via ranged manifest requests. In practice, this is rare; most nodes come back online within hours or days.

---

## Comparison with Podping

| Aspect | Podping (Hive) | Gossipping (Iroh) |
|--------|----------------|-------------------|
| **Transport** | Hive blockchain | Iroh gossip (QUIC/P2P) |
| **Latency** | ~3-20 seconds (block time) | Sub-second (direct gossip) |
| **Write cost** | Hive RC (free but requires stake) | Zero (just bandwidth) |
| **Account needed** | Hive account + stake | Ed25519 keypair (auto-generated) |
| **Persistence** | Permanent (on-chain) | Every node archives to object storage |
| **Ordering** | Global (block order) | None (eventually consistent) |
| **Infrastructure** | Hive nodes | Relay nodes (lightweight) |
| **Spam resistance** | Transaction cost | Signatures + registry + rate limits |
| **NAT traversal** | N/A (HTTP API) | Built-in (Iroh relay + hole-punching) |

| **NAT traversal** | N/A (HTTP API) | Built-in (Iroh relay + hole-punching) |

## Archival & Catch-Up

### Design Principle

Every node in the network — publisher, consumer, or relay — archives every valid message it receives to S3-compatible object storage. This makes the entire network a distributed, redundant archive. Any node can serve as a catch-up source for any other node.

### Object Storage Layout

Each node writes messages to its own object storage bucket (S3, R2, MinIO, local filesystem with S3 API, etc.) using a time-partitioned key scheme:

```
gossipping/
  v1/
    2026/
      02/
        21/
          14/
            <message_hash>.json      ← individual message
          14.manifest.jsonl           ← hour manifest (append-only)
        21.manifest.jsonl             ← day manifest
```

**Message key**: `gossipping/v1/{year}/{month}/{day}/{hour}/{message_hash}.json`

**Message hash**: BLAKE3 hash of the canonical signed message bytes (compact JSON, sorted keys, including signature). This guarantees content-addressable deduplication — the same notification always produces the same key, regardless of which node archived it.

**Manifest files**: Append-only JSONL files listing every message hash archived in that time period, along with minimal metadata for filtering without fetching the full message:

```jsonl
{"hash":"abc123...","sender":"def456...","medium":"podcast","reason":"update","timestamp":1708531200,"iri_count":3}
{"hash":"ghi789...","sender":"jkl012...","medium":"podcast","reason":"live","timestamp":1708531205,"iri_count":1}
```

### Storage Requirements

Gossipping messages are tiny. Back-of-envelope math:

- Average message size: ~500 bytes
- Current Podping volume: ~50,000 notifications/day
- Daily storage: ~25 MB
- Monthly storage: ~750 MB
- Yearly storage: ~9 GB

Even with 10x growth, this is trivially cheap on any object storage provider.

### Catch-Up Protocol

When a node comes online after being offline, it needs to fill in the messages it missed. The catch-up protocol uses a **manifest exchange** over Iroh's QUIC streams:

```
┌──────────┐                          ┌──────────┐
│ Returning│                          │  Peer    │
│   Node   │                          │ (online) │
└────┬─────┘                          └────┬─────┘
     │                                     │
     │  1. JOIN gossip topic               │
     │────────────────────────────────────►│
     │                                     │
     │  2. CATCHUP_REQUEST                 │
     │     { since: 1708444800 }           │
     │────────────────────────────────────►│
     │                                     │
     │  3. CATCHUP_RESPONSE                │
     │     { manifests: [hour manifests]}  │
     │◄────────────────────────────────────│
     │                                     │
     │  4. REQUEST_MESSAGES                │
     │     { hashes: [missing hashes] }    │
     │────────────────────────────────────►│
     │                                     │
     │  5. MESSAGES                         │
     │     { messages: [...] }             │
     │◄────────────────────────────────────│
     │                                     │
```

**Steps:**

1. The returning node joins the gossip topic and begins receiving live messages immediately.
2. It opens a QUIC stream (using a custom ALPN: `gossipping/catchup/v1`) to one or more peers and sends a `CATCHUP_REQUEST` with the timestamp of its last known message.
3. The peer responds with hour-level manifest entries covering the requested time range.
4. The returning node diffs the received manifest against its own archive and requests only the missing messages by hash.
5. The peer streams back the full message bodies.

**Catch-up is idempotent**: Because messages are keyed by content hash, receiving the same message twice is a no-op. Nodes can catch up from multiple peers in parallel for speed and resilience.

### Catch-Up Custom Protocol

The catch-up protocol is implemented as a separate Iroh protocol handler alongside gossip:

```rust
const CATCHUP_ALPN: &[u8] = b"gossipping/catchup/v1";

#[derive(Debug, Clone)]
struct CatchupProtocol {
    archive: Arc<Archive>,  // the node's local object storage
}

impl ProtocolHandler for CatchupProtocol {
    async fn accept(&self, connection: Connection) -> Result<()> {
        let (mut send, mut recv) = connection.accept_bi().await?;

        // Read the catchup request
        let request: CatchupRequest = read_request(&mut recv).await?;

        // Send manifests for the requested time range
        let manifests = self.archive
            .get_manifests_since(request.since)
            .await?;
        write_response(&mut send, &CatchupResponse { manifests }).await?;

        // Read which specific messages they need
        let needed: MessageRequest = read_request(&mut recv).await?;

        // Stream the messages
        for hash in needed.hashes {
            if let Some(msg) = self.archive.get_message(&hash).await? {
                write_message(&mut send, &msg).await?;
            }
        }

        send.finish()?;
        Ok(())
    }
}
```

The router registers both protocols:

```rust
let router = Router::builder(endpoint.clone())
    .accept(iroh_gossip::ALPN, gossip.clone())
    .accept(CATCHUP_ALPN, catchup_protocol.clone())
    .spawn();
```

### Retention Policy

Nodes are free to set their own retention window. Recommended defaults:

| Role | Retention | Rationale |
|------|-----------|-----------|
| **Relay** | 30 days | Relays are the backbone; longer retention helps late joiners |
| **Consumer** | 7 days | Most apps check in at least weekly |
| **Publisher** | 24 hours | Publishers mainly care about sending, not serving history |

Nodes SHOULD advertise their retention window in catch-up responses so requesters know whether to try additional peers for older messages.

### Archive Storage Backends

The archive layer uses a simple trait so nodes can choose their backend:

```rust
#[async_trait]
trait ArchiveStorage: Send + Sync {
    /// Store a message, keyed by its BLAKE3 hash
    async fn put_message(&self, hash: &str, message: &[u8]) -> Result<()>;

    /// Retrieve a message by hash
    async fn get_message(&self, hash: &str) -> Result<Option<Vec<u8>>>;

    /// Append an entry to the hour manifest
    async fn append_manifest(&self, hour_key: &str, entry: &ManifestEntry) -> Result<()>;

    /// Get manifest entries for a time range
    async fn get_manifests_since(&self, since: u64) -> Result<Vec<ManifestEntry>>;
}
```

Built-in implementations:

- **`S3Storage`**: Any S3-compatible service (AWS S3, Cloudflare R2, MinIO, Garage).
- **`FsStorage`**: Local filesystem with the same key structure. Good for development and small deployments.
- **`SqliteStorage`**: Single-file SQLite database. Simplest option for nodes that don't want to run an object store.

---

## Optional Extensions

### WebSocket Bridge

For browser-based consumers or systems that can't run Iroh natively, a bridge node can expose gossip messages over WebSocket:

```
Consumer (browser) ←WebSocket→ Bridge Node ←Iroh Gossip→ Swarm
```

### `<podcast:gossipping>` RSS Tag

A new namespace tag to signal that a feed uses Gossipping:

```xml
<podcast:gossipping
  publicKey="abc123..."
  topic="gossipping/v1/all" />
```

This tells consumers they can expect real-time notifications from this publisher and allows them to verify the publisher's identity.

### Metrics & Monitoring

Relay operators can expose Prometheus metrics for network health: peer count, messages/second, unique publishers seen, message verification failure rate, etc.

---

## Integrating with Podping.cloud

### Existing Podping.cloud Architecture

The current podping.cloud codebase has a clean two-component design that makes integration straightforward:

```
┌─────────────────────────────────────────────────────────────────┐
│                     CURRENT PODPING.CLOUD                       │
│                                                                 │
│   Publisher                                                     │
│      │                                                          │
│      │  GET /?url=...&reason=...&medium=...                     │
│      │  Authorization: Bearer <token>                           │
│      ▼                                                          │
│   ┌─────────────────────────────────┐                           │
│   │   Rust HTTP Front-End           │                           │
│   │                                 │                           │
│   │  1. Validate auth token         │                           │
│   │     (auth.db SQLite)            │                           │
│   │  2. Validate URL format         │                           │
│   │  3. Queue in queue.db           │                           │
│   │  4. Return 200 OK              │                           │
│   │                                 │                           │
│   │  ┌───────────────────────────┐  │                           │
│   │  │ Queue Checker Thread      │  │                           │
│   │  │                           │  │                           │
│   │  │ • Poll queue.db (FIFO)    │  │                           │
│   │  │ • Prioritize "live"       │  │                           │
│   │  │ • Batch up to 1000 feeds  │  │                           │
│   │  │ • Send Cap'n Proto Ping   │  │                           │
│   │  │   over ZMQ socket         │  │                           │
│   │  └─────────┬─────────────────┘  │                           │
│   └────────────┼────────────────────┘                           │
│                │                                                │
│                │  ZMQ TCP :9999                                  │
│                │  (Cap'n Proto Ping objects)                     │
│                ▼                                                │
│   ┌─────────────────────────────────┐                           │
│   │   Python hive-writer            │                           │
│   │                                 │                           │
│   │  • Receive Ping from ZMQ        │                           │
│   │  • Write custom JSON to Hive    │                           │
│   │  • Return success/error         │                           │
│   └─────────────────────────────────┘                           │
│                │                                                │
│                ▼                                                │
│         Hive Blockchain                                         │
└─────────────────────────────────────────────────────────────────┘
```

The critical insight: **the ZMQ socket between the front-end and the hive-writer is a clean integration seam**. The front-end doesn't know or care what the back-end does with the Ping objects. It just sends them and waits for success/error. This was intentionally designed to support future transport backends.

### Integration Strategy: Three Options

#### Option A: Gossip-Writer as a New Back-End (Lowest Risk)

Write a new Rust process — `gossip-writer` — that speaks the same ZMQ + Cap'n Proto protocol as hive-writer but broadcasts into the Iroh gossip swarm instead. The front-end is untouched.

```
                    OPTION A: REPLACE
                    ─────────────────

   Publisher
      │
      │  GET /?url=...
      ▼
   ┌──────────────────────┐
   │  Rust HTTP Front-End  │     ◄── UNCHANGED
   │  (podping.cloud)      │
   └──────────┬───────────┘
              │
              │  ZMQ :9999 (Cap'n Proto)
              ▼
   ┌──────────────────────┐
   │  gossip-writer (NEW)  │     ◄── NEW COMPONENT
   │                       │
   │  • Receive Ping       │
   │  • Build notification │
   │  • Sign (ed25519)     │
   │  • Archive (S3/SQLite)│
   │  • Broadcast (gossip) │
   └──────────┬───────────┘
              │
              ▼
        Iroh Gossip Swarm
```

**Pros**: Zero modifications to existing code. Drop-in replacement. Can swap back to hive-writer at any time.

**Cons**: Loses Hive publishing. Two separate processes to manage. The gossip-writer doesn't benefit from the front-end's Rust ecosystem (has to re-implement Iroh setup separately).

#### Option B: Dual-Write with Fan-Out (Recommended for Transition)

Modify the queue checker thread in the Rust front-end to fan out to **two** ZMQ sockets — the existing hive-writer AND a new gossip-writer. Every notification goes to both Hive and the gossip swarm.

```
                    OPTION B: DUAL-WRITE
                    ────────────────────

   Publisher
      │
      │  GET /?url=...
      ▼
   ┌──────────────────────────────────────┐
   │  Rust HTTP Front-End                  │
   │  (podping.cloud)                      │
   │                                       │
   │  ┌────────────────────────────────┐   │
   │  │ Queue Checker Thread           │   │
   │  │                                │   │   ◄── MODIFIED:
   │  │ • Poll queue.db (FIFO)         │   │       fan-out to
   │  │ • Batch up to 1000 feeds       │   │       two sockets
   │  │ • Send to BOTH writers         │   │
   │  └───────┬───────────┬────────────┘   │
   │          │           │                │
   └──────────┼───────────┼────────────────┘
              │           │
     ZMQ :9999│           │ZMQ :9998
              │           │
              ▼           ▼
   ┌──────────────┐  ┌──────────────────┐
   │ hive-writer   │  │ gossip-writer    │
   │ (EXISTING)    │  │ (NEW)            │
   │               │  │                  │
   │ → Hive chain  │  │ → Iroh gossip    │
   │               │  │ → Archive (S3)   │
   └──────────────┘  └──────────────────┘
        │                    │
        ▼                    ▼
  Hive Blockchain    Gossip Swarm + Archive
```

The front-end change is minimal — just add a second ZMQ socket to the queue checker's send loop:

```rust
// BEFORE (current code, simplified):
let hive_socket = zmq_connect("tcp://localhost:9999")?;
for ping in batch {
    hive_socket.send(ping.serialize())?;
    let result = hive_socket.recv()?;
    if result.is_success() {
        remove_from_queue(ping.id)?;
    }
}

// AFTER (dual-write):
let hive_socket = zmq_connect("tcp://localhost:9999")?;
let gossip_socket = zmq_connect("tcp://localhost:9998")?;

for ping in batch {
    let payload = ping.serialize();

    // Send to both writers. Gossip is fire-and-forget;
    // Hive remains the authoritative success signal.
    let gossip_result = gossip_socket.send(&payload);
    if let Err(e) = gossip_result {
        log::warn!("Gossip write failed: {e}");
        // Don't block — gossip failure shouldn't affect the main path
    }

    hive_socket.send(&payload)?;
    let result = hive_socket.recv()?;
    if result.is_success() {
        remove_from_queue(ping.id)?;
    }
}
```

**Pros**: Zero downtime transition. Hive remains the fallback. Gossip network can be tested in production without risk. Publishers don't change anything.

**Cons**: Two back-end processes. The gossip-writer is still a separate process. Small modification to the front-end.

#### Option C: Embedded Gossip (Recommended Long-Term)

Add the Iroh gossip layer directly into the Rust front-end binary. Since podping.cloud is already Rust and already has the HTTP server, auth database, and queue logic, the gossip functionality becomes just another output path — no ZMQ, no separate process.

```
                    OPTION C: EMBEDDED
                    ──────────────────

   Publisher
      │
      │  GET /?url=...                    Consumer (SSE)
      │  POST /api/v1/podping             │
      ▼                                   │ GET /api/v1/firehose
   ┌──────────────────────────────────────────────────────────┐
   │                                                          │
   │   podping.cloud (extended)                               │
   │                                                          │
   │   ┌─────────────────────────────────────────────────┐    │
   │   │              HTTP Server (existing + new)        │    │
   │   │                                                  │    │
   │   │  GET /?url=...          (existing, unchanged)    │    │
   │   │  POST /api/v1/podping   (new: batch endpoint)    │    │
   │   │  GET /api/v1/notifications  (new: archive query) │    │
   │   │  GET /api/v1/firehose       (new: SSE stream)    │    │
   │   │  GET /api/v1/status         (new: node status)   │    │
   │   └────────────────────┬────────────────────────────┘    │
   │                        │                                  │
   │                        ▼                                  │
   │   ┌────────────────────────────────────────────────┐     │
   │   │           Queue Checker Thread                  │     │
   │   │                                                 │     │
   │   │  • Poll queue.db (FIFO)                         │     │
   │   │  • Batch up to 1000 feeds                       │     │
   │   │  • Fan out to all enabled writers:              │     │
   │   │                                                 │     │
   │   │    ┌──────────┐  ┌──────────┐  ┌────────────┐  │     │
   │   │    │   Hive   │  │  Gossip  │  │  Archive   │  │     │
   │   │    │  Writer  │  │  Writer  │  │  Writer    │  │     │
   │   │    │  (ZMQ)   │  │ (in-proc)│  │ (in-proc)  │  │     │
   │   │    └────┬─────┘  └────┬─────┘  └─────┬──────┘  │     │
   │   │         │             │               │         │     │
   │   └─────────┼─────────────┼───────────────┼─────────┘     │
   │             │             │               │               │
   │             │             │               ▼               │
   │             │             │     ┌──────────────────┐      │
   │             │             │     │  Archive Storage │      │
   │             │             │     │  (S3 / SQLite)   │      │
   │             │             │     └──────────────────┘      │
   │             │             │                               │
   │             │             ▼                               │
   │             │     ┌──────────────────┐                    │
   │             │     │  Iroh Endpoint   │                    │
   │             │     │  ┌────────────┐  │                    │
   │             │     │  │  Gossip    │  │                    │
   │             │     │  │  Protocol  │  │                    │
   │             │     │  ├────────────┤  │                    │
   │             │     │  │  Catch-Up  │  │                    │
   │             │     │  │  Protocol  │  │                    │
   │             │     │  └────────────┘  │                    │
   │             │     └───────┬──────────┘                    │
   │             │             │                               │
   └─────────────┼─────────────┼───────────────────────────────┘
                 │             │
                 ▼             ▼
           Hive Blockchain   Gossip Swarm
           (optional)        (primary)
```

**Pros**: Single binary. No ZMQ overhead for the gossip path. All Gossipping features (archive, catch-up, SSE firehose, batch API) are natively integrated. Can still keep the hive-writer as an optional output via ZMQ. The cleanest long-term architecture.

**Cons**: Larger change to the existing codebase. Requires adding `iroh`, `iroh-gossip`, `axum` (or extending the existing HTTP server), and archive storage dependencies to the Rust project.

### Recommended Migration Plan

The three options map naturally to migration phases:

```
Phase 1                    Phase 2                    Phase 3
(weeks 1-2)                (weeks 3-6)                (weeks 7+)
─────────────────────      ─────────────────────      ─────────────────────

Option B: Dual-Write       Option C: Embedded         Hive Optional
                                                      
┌────────────┐             ┌────────────────────┐     ┌────────────────────┐
│ podping.cloud│            │ podping.cloud       │     │ podping.cloud       │
│ front-end   │            │ (extended)           │     │ (extended)           │
│ (tiny mod)  │            │                      │     │                      │
└──┬──────┬──┘             │ Gossip + Archive     │     │ Gossip + Archive     │
   │      │                │ + Catch-Up + SSE     │     │ + Catch-Up + SSE     │
   │      │                │ + Hive (via ZMQ)     │     │                      │
   ▼      ▼                └──────────────────────┘     │ Hive: OFF by default │
 Hive   Gossip                                          └──────────────────────┘
         (via ZMQ                      
          gossip-writer)   
```

**Phase 1: Dual-Write (Option B)**

Goal: Get gossip notifications flowing alongside Hive with minimal code changes.

1. Write the `gossip-writer` as a standalone Rust binary that listens on ZMQ port 9998.
2. Add a second ZMQ socket to the podping.cloud queue checker thread.
3. Deploy gossip-writer alongside hive-writer in the Docker Compose stack.
4. Stand up 2-3 relay nodes to seed the gossip swarm.
5. Invite a few consumers (Podcast Index, Podping.watch) to connect and verify.

Changes to podping.cloud: ~50 lines (add second ZMQ socket, config flag).

```yaml
# docker-compose.yml addition
services:
  podping:
    # ... existing config ...
    environment:
      - GOSSIP_WRITER_ENABLED=true
      - GOSSIP_WRITER_ZMQ=tcp://gossip-writer:9998

  hive-writer:
    # ... existing config, unchanged ...

  gossip-writer:           # NEW
    image: gossipping/writer:latest
    ports:
      - "9998:9998"        # ZMQ from front-end
      - "8081:8081"        # Gossipping HTTP API
    environment:
      - IROH_SECRET_FILE=/data/iroh.key
      - ARCHIVE_BACKEND=sqlite
      - ARCHIVE_PATH=/data/archive.db
      - BOOTSTRAP_RELAYS=https://gossipping.org/relays.json
    volumes:
      - ./data:/data
```

**Phase 2: Embedded (Option C)**

Goal: Merge gossip functionality into the podping.cloud binary for a cleaner architecture.

1. Add `iroh`, `iroh-gossip`, `blake3`, and archive storage crates to podping.cloud's `Cargo.toml`.
2. Initialize the Iroh endpoint and gossip subscription on startup.
3. Replace the ZMQ gossip-writer with an in-process gossip broadcast in the queue checker.
4. Add the new REST endpoints (`/api/v1/podping`, `/api/v1/notifications`, `/api/v1/firehose`, `/api/v1/status`).
5. Add the catch-up protocol handler to the Iroh router.
6. Retire the standalone gossip-writer container.

The Hive path remains available via ZMQ but becomes a config toggle:

```toml
# podping.toml
[writers]
hive.enabled = true        # Keep Hive during transition
hive.zmq = "tcp://localhost:9999"

gossip.enabled = true      # Iroh gossip (in-process)
gossip.topic = "gossipping/v1/all"
gossip.bootstrap = "https://gossipping.org/relays.json"

[archive]
backend = "s3"             # or "sqlite", "fs"
s3.bucket = "podping-archive"
s3.region = "us-east-1"
retention_days = 30

[http]
port = 8080
sse_enabled = true
```

**Phase 3: Hive Optional**

Goal: Gossip is the primary transport. Hive is kept as an optional, secondary output for operators who want on-chain permanence.

1. Default `hive.enabled = false` in new installations.
2. Existing operators can keep Hive enabled indefinitely — it's just a config flag.
3. The Docker Compose stack no longer includes hive-writer by default.
4. Documentation points to gossip as the primary path.

### What Stays the Same

Throughout all phases, these things **never change**:

- **Publisher-facing API**: `GET /?url=...&reason=...&medium=...` with `Authorization: Bearer` header. Publishers don't modify anything.
- **Auth database**: Same `auth.db` SQLite database with the same publisher token records.
- **Queue database**: Same `queue.db` with the same FIFO queue logic.
- **Docker deployment model**: Still `docker compose up`.

### What Gets Added

| Component | Phase 1 | Phase 2 | Phase 3 |
|-----------|---------|---------|---------|
| gossip-writer (standalone) | ✅ New container | ❌ Removed | ❌ Removed |
| Iroh gossip (in-process) | ❌ | ✅ Embedded | ✅ Primary |
| Hive writer | ✅ Unchanged | ✅ Optional | ⚪ Off by default |
| Archive storage | ✅ In gossip-writer | ✅ Embedded | ✅ Embedded |
| Catch-up protocol | ✅ In gossip-writer | ✅ Embedded | ✅ Embedded |
| SSE firehose | ❌ | ✅ New endpoint | ✅ |
| Batch POST API | ❌ | ✅ New endpoint | ✅ |
| Archive query API | ❌ | ✅ New endpoint | ✅ |

### Concrete Code Changes to podping.cloud

#### Phase 1: Minimal (~50 lines changed)

**`Cargo.toml`** — no changes needed (ZMQ is already a dependency).

**Queue checker thread** — add a second socket:

```rust
// In the queue checker loop, after building the batch:

// Existing: send to hive-writer
if config.hive_enabled {
    match hive_socket.send(&ping_bytes, 0) {
        Ok(_) => { /* wait for response, remove from queue */ }
        Err(e) => log::error!("Hive write failed: {e}"),
    }
}

// New: also send to gossip-writer (fire-and-forget)
if config.gossip_writer_enabled {
    if let Err(e) = gossip_socket.send(&ping_bytes, zmq::DONTWAIT) {
        log::warn!("Gossip write failed: {e}");
        // Don't block the main loop — gossip is best-effort during Phase 1
    }
}
```

#### Phase 2: Medium (~500-800 lines added)

**`Cargo.toml`** — add dependencies:

```toml
[dependencies]
iroh = "0.35"
iroh-gossip = "0.96"
blake3 = "1"
# ... plus archive storage crate of choice
```

**`main.rs`** — initialize Iroh on startup:

```rust
// After existing HTTP server setup:

let iroh_endpoint = Endpoint::builder()
    .secret_key(load_or_generate_iroh_key(&config)?)
    .add_discovery(PkarrResolver::n0_dns())
    .add_discovery(DhtDiscovery::builder().build()?)
    .bind()
    .await?;

let gossip = Gossip::builder().spawn(iroh_endpoint.clone());
let archive = init_archive(&config)?;
let catchup = CatchupProtocol { archive: archive.clone() };

let _iroh_router = Router::builder(iroh_endpoint.clone())
    .accept(iroh_gossip::ALPN, gossip.clone())
    .accept(CATCHUP_ALPN, catchup)
    .spawn();

let topic = TopicId::from_bytes(sha256(b"gossipping/v1/all"));
let bootstrap = discover_bootstrap_peers(&config).await?;
let (gossip_sender, gossip_receiver) = gossip
    .subscribe(topic, bootstrap)
    .await?;

// Spawn gossip receiver → archive task
tokio::spawn(gossip_receive_loop(gossip_receiver, archive.clone()));

// Pass gossip_sender to the queue checker thread
// Pass archive to the new HTTP endpoints
```

**New HTTP routes** — extend the existing HTTP server:

```rust
// Add alongside existing GET / route:
.route("/api/v1/podping", post(handle_podping_batch))
.route("/api/v1/notifications", get(handle_query_notifications))
.route("/api/v1/firehose", get(handle_firehose_sse))
.route("/api/v1/status", get(handle_status))
```

### Backward Compatibility

The integration is fully backward-compatible at every layer:

- **Publishers**: No changes required. Same GET endpoint, same auth tokens.
- **Hive consumers** (podping-hivewatcher, podping-hivewatcher-js): Continue to work unchanged as long as hive-writer is enabled.
- **New gossip consumers**: Connect to any node's gossip swarm or HTTP API.
- **Docker operators**: Phase 1 just adds a container. Phase 2 removes it. Phase 3 removes hive-writer.

The podping.cloud codebase was designed with extensibility in mind — the ZMQ writer interface is the proof. Gossipping slots into that interface naturally, and the migration path lets operators move at their own pace without breaking anything.

---

## Open Questions

1. **Message ordering**: Gossip provides no global order. Is this a problem? (Probably not — consumers are already tolerant of out-of-order feed fetches.)
2. **Default retention per role**: The design suggests 30d/7d/24h for relay/consumer/publisher — are these the right defaults?
3. **Topic splitting threshold**: At what message volume should we split into per-medium topics?
4. **Registry governance**: Who maintains the publisher registry? A foundation? The Podcast Index?
5. **Mobile consumers**: Can podcast apps on phones realistically maintain a gossip connection + local archive, or do they need the WebSocket bridge?
6. **Storage backend defaults**: Should the default be SQLite (simplest) or S3 (most scalable)? Or auto-detect based on configuration?
7. **Catch-up peer selection**: How does a returning node decide which peers to request catch-up from? Random? Prefer relays? Ask multiple in parallel?
8. **Manifest compaction**: Should day-level manifests be compacted from hour-level manifests, or generated independently?

---

## Getting Started

```bash
# Add dependencies
cargo add iroh iroh-gossip serde serde_json ed25519-dalek sha2 blake3 tokio axum

# Run a relay (gossip + HTTP on :8080)
cargo run --bin relay

# Run a consumer (gossip + HTTP on :8080)
cargo run --bin consumer

# Send a test notification via gossip
cargo run --bin publisher -- --feed "https://example.com/feed.rss"

# Send a test notification via HTTP (podping.cloud-compatible)
curl "http://localhost:8080/?url=https://example.com/feed.rss&reason=update&medium=podcast" \
  -H "Authorization: Bearer test-token"

# Send a batch notification via HTTP
curl -X POST "http://localhost:8080/api/v1/podping" \
  -H "Authorization: Bearer test-token" \
  -H "Content-Type: application/json" \
  -d '{"reason":"update","medium":"podcast","iris":["https://example.com/feed1.rss","https://example.com/feed2.rss"]}'

# Query the archive
curl "http://localhost:8080/api/v1/notifications?since=1708444800&medium=podcast"

# Stream the firehose (SSE)
curl -N "http://localhost:8080/api/v1/firehose"
```