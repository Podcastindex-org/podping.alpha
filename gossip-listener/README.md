# gossip-listener

A peer-to-peer subscriber that receives podcast feed update notifications over [Iroh](https://iroh.computer/) gossip. Part of the [Podping.alpha](../README.md) project.

## What it does

`gossip-listener` joins the `gossipping/v1/all` gossip topic, discovers peers via DHT and a local bootstrap list, and prints incoming `GossipNotification` messages to stdout. Each notification carries a list of podcast feed IRIs, a medium/reason classification, and an ed25519 signature that the listener verifies before display.

The listener also participates in the protocol's trust and discovery layers:

- **Peer discovery** -- saves `PeerAnnounce` and `NeighborUp` node IDs to a local file so future runs can bootstrap without DHT.
- **Trust propagation** -- accepts signed `PeerEndorse` messages from already-trusted senders and dynamically expands the trusted publisher set.
- **Watchdog** -- if no notification arrives within 180 seconds, automatically re-bootstraps from known peers.

## Building

### With Nix

```sh
nix build .#gossip-listener
./result/bin/gossip-listener
```

Or run directly:

```sh
nix run
```

### With Cargo

```sh
cargo build --release --manifest-path gossip-listener/Cargo.toml
./gossip-listener/target/release/gossip-listener
```

### With prebuilt binaries (GitHub Releases)

When you tag a release (e.g., `v1.2.3`), the GitHub Action publishes binaries
for Linux, macOS, and ~Windows~  under the release assets.

Download and verify:
```sh
VERSION="v0.5.1"
OS=$(uname -s) # linux | macos
[ "${OS,,}" = darwin ] && OS="macos" 
ARCH=x86_64 # x86_64 | aarch64 | armv7
BINARY="gossip-listener-${VERSION}-${OS,,}-${ARCH}.tar.gz"
URL="https://github.com/Podcastindex-org/podping.alpha/releases/download/${VERSION}/${BINARY}"
SHA_URL="${URL}.sha256"
curl -L "$URL" -O
curl -L "$SHA_URL" -O
shasum -a 256 -c ${BINARY}.sha256 && tar -xzf ${BINARY}
export NODE_FRIENDLY_NAME='SOMETHING_FANCY'
./gossip-listener
```

Windows (PowerShell):
```powershell
$Version = "v1.2.3"
$Url = "https://github.com/<owner>/<repo>/releases/download/$Version/gossip-listener-$Version-windows-x86_64.zip"
$ShaUrl = "$Url.sha256"
Invoke-WebRequest -Uri $Url -OutFile gossip-listener.zip
Invoke-WebRequest -Uri $ShaUrl -OutFile gossip-listener.zip.sha256
$Expected = (Get-Content gossip-listener.zip.sha256).Split(' ')[0].ToLower()
$Actual = (Get-FileHash -Algorithm SHA256 -Path gossip-listener.zip).Hash.ToLower()
if ($Expected -ne $Actual) { throw "SHA256 mismatch" }
Expand-Archive -Path gossip-listener.zip -DestinationPath .
./gossip-listener.exe
```

### With Docker

Build locally from this repo:

```sh
docker build -f docker/Dockerfile.gossip-listener -t gossip-listener:local .
docker run --rm -it \
  -v "$(pwd)":/data \
  gossip-listener:local
```

Pull the published image from GHCR:

```sh
docker pull ghcr.io/<owner>/gossip-listener:latest
docker run --rm -it \
  -v "$(pwd)":/data \
  ghcr.io/<owner>/gossip-listener:latest
```

The container runs as a non-root user (UID 1000) with `/data` as the working
directory, so mount a writable volume if you want to persist node keys and
known peers.

### With Docker Compose

Use the dedicated compose file:

```sh
docker compose -f docker/docker-compose.gossip-listener.yaml up -d
```

Data is stored under `./data/gossip-listener` by default, and the compose file
sets the `*_FILE` environment variables to `/data/...` paths for persistence.

## Configuration

All configuration is via environment variables. Every variable is optional.

| Variable | Default | Description |
|---|---|---|
| `BOOTSTRAP_PEER_IDS` | *(empty)* | Comma-separated Iroh node IDs to join on startup |
| `IROH_NODE_KEY_FILE` | `gossip_listener_node.key` | Path to persistent node identity key |
| `KNOWN_PEERS_FILE` | `gossip_listener_known_peers.txt` | Path to read/write discovered peer IDs (max 15) |
| `DHT_INITIAL_SECRET` | `podping_gossip_default_secret` | Shared secret for DHT auto-discovery |
| `TRUSTED_PUBLISHERS_FILE` | `trusted_publishers.txt` | Path to a file of trusted ed25519 public keys (hex, one per line). Empty = accept all |
| `NODE_FRIENDLY_NAME` | *(none)* | Optional human-readable node name (max 64 chars), broadcast in PeerAnnounce/PeerEndorse |
| `PEER_ANNOUNCE_INTERVAL` | `300` | Seconds between broadcasting `PeerAnnounce` messages |
| `PEER_ENDORSE_INTERVAL` | `45` | Seconds between broadcasting `PeerEndorse` messages |
| `SSE_ENABLED` | `false` | Enable the Server-Sent Events HTTP endpoint (`true`, `1`, or `yes`) |
| `SSE_BIND_ADDR` | `0.0.0.0:8089` | Address and port for the SSE server |
| `SSE_BUFFER_SIZE` | `1000` | Broadcast channel capacity for SSE clients |

## Output format

Verified notifications are printed as single-line JSON prefixed with `PODPING:`:

```
PODPING: [{"version":"1.1","sender":"ab12...","timestamp":1709827200,"medium":"podcast","reason":"update","iris":["https://example.com/feed.xml"],"signature":"cd34...","sig_status":"VALID"}]
```

The `sig_status` field is one of `VALID`, `UNSIGNED`, or `INVALID`.

## SSE (Server-Sent Events)

When `SSE_ENABLED=true`, the listener runs an HTTP server that streams podping notifications as Server-Sent Events. External applications can subscribe to receive notifications in real-time.

**Endpoints:**

| Route | Description |
|-------|-------------|
| `GET /` | Health check — returns `gossip-listener SSE v{version}` |
| `GET /events` | SSE stream of podping notifications |

**Query filters on `/events` (all optional, AND'd):**

| Param | Example | Description |
|-------|---------|-------------|
| `medium` | `podcast` | Only notifications with this medium |
| `reason` | `update` | Only notifications with this reason |
| `sender` | `abcd1234` | Only notifications from senders whose pubkey starts with this prefix |

**Example:**

```sh
# Stream all notifications
curl -N http://localhost:8089/events

# Stream only podcast updates
curl -N "http://localhost:8089/events?medium=podcast&reason=update"
```

Each event is delivered as:

```
event: podping
data: {"version":"1.0","sender":"abcd1234...","medium":"podcast","reason":"update","iris":[...],"sig_status":"VALID","sender_name":"..."}
```

JavaScript clients can use `EventSource`:

```js
const es = new EventSource("http://localhost:8089/events?medium=podcast");
es.addEventListener("podping", (e) => {
  const notification = JSON.parse(e.data);
  console.log(notification.iris);
});
```

## Trusted publisher filtering

If `trusted_publishers.txt` (or the path set by `TRUSTED_PUBLISHERS_FILE`) contains one or more hex-encoded ed25519 public keys, only notifications from those senders are displayed. The set can grow at runtime when a trusted peer broadcasts a signed `PeerEndorse` message vouching for additional keys.

If the file is empty or missing, all notifications are accepted.

## License

[MIT](../LICENSE)
