#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
MODE="${1:-debug}"

if [ "$MODE" = "release" ]; then
    CARGO_FLAGS="--release"
    echo "=== Building all modules (release) ==="
else
    CARGO_FLAGS=""
    echo "=== Building all modules (debug) ==="
fi

echo ""
echo "--- podping (front-end) ---"
cargo build $CARGO_FLAGS --manifest-path "$ROOT/podping/Cargo.toml"

echo ""
echo "--- gossip-writer ---"
cargo build $CARGO_FLAGS --manifest-path "$ROOT/gossip-writer/Cargo.toml"

echo ""
echo "--- stresser ---"
cargo build $CARGO_FLAGS --manifest-path "$ROOT/stresser/Cargo.toml"

echo ""
echo "=== All modules built successfully ==="
