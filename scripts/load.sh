#!/usr/bin/env bash
# HTTP load test for `ziv serve`: starts the server on a fixed port against the committed
# multi-tile fixture, then runs `oha` against a URL list mixing cache-hit (the same tile
# requested repeatedly) and cache-miss (a spread of distinct tiles) traffic, and writes the
# results under bench/results/.
#
# This is a MANUAL/OPTIONAL script, not a `cargo test` -- `oha` is an external load-testing
# binary, not a Rust test dependency, so it can't run in `cargo test --workspace` or a default CI
# job. Install it first:
#   cargo install oha
#   # or: brew install oha
#
# Usage:
#   ./scripts/load.sh
#
# Why the multi-tile fixture, not a 64x64 one: every fixture used by the unit/integration test
# suite except `sample_multi_tile.ome.zarr` is <=64x64 pixels, i.e. smaller than a single 512px
# IIIF tile -- every request against those degenerates to the SAME one full-image tile, which
# can't produce a meaningful cache-hit/cache-miss mix or realistic per-tile render cost. This
# script always points at `tests/fixtures/sample_multi_tile.ome.zarr` (a 1024x1024, two-level,
# genuinely multi-tile pyramid) so the hot/cold URL mix below actually exercises distinct tiles.
set -euo pipefail

if ! command -v oha >/dev/null 2>&1; then
    echo "error: oha not found on PATH." >&2
    echo "install it with: cargo install oha   (or: brew install oha)" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE="$REPO_ROOT/tests/fixtures/sample_multi_tile.ome.zarr"
PORT="3099"
ADDR="127.0.0.1:$PORT"
BASE_URL="http://$ADDR"
RESULTS_DIR="$REPO_ROOT/bench/results"
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"

if [ ! -d "$FIXTURE" ]; then
    echo "error: fixture not found at $FIXTURE" >&2
    echo "generate it with: cargo test -p ziv-exporter --test build_multi_tile_fixture" >&2
    exit 1
fi

mkdir -p "$RESULTS_DIR"

echo "building ziv (release) ..."
cargo build --release -p ziv --manifest-path "$REPO_ROOT/Cargo.toml"

echo "starting ziv serve on $ADDR against $FIXTURE ..."
"$REPO_ROOT/target/release/ziv" serve "$FIXTURE" --addr "$ADDR" &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true' EXIT

# Wait for the server to accept connections (info.json is always the first thing up).
for _ in $(seq 1 50); do
    if curl -fs "$BASE_URL/iiif/default/info.json" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

# --- cache-hit run: the SAME tile requested repeatedly, over and over. ---
HOT_URL="$BASE_URL/iiif/default/0,0,512,512/512,512/0/default.jpg"
echo "cache-hit (hot) run: $HOT_URL"
oha -z 30s -c 50 --output-format json --no-tui "$HOT_URL" >"$RESULTS_DIR/hot-$TIMESTAMP.json"

# --- cache-miss run: oha itself only takes one URL per invocation, so cache-miss traffic is
# approximated by cycling through a handful of DISTINCT tile URLs across the level-0 2x2 tile
# grid the multi-tile fixture provides, each run separately and concatenated into one report.
# A real varied-URL generator would need a proxy in front of oha; documenting the tile list here
# keeps the script a single dependency (oha) and reproducible.
COLD_URLS=(
    "$BASE_URL/iiif/default/0,0,512,512/512,512/0/default.jpg"
    "$BASE_URL/iiif/default/512,0,512,512/512,512/0/default.jpg"
    "$BASE_URL/iiif/default/0,512,512,512/512,512/0/default.jpg"
    "$BASE_URL/iiif/default/512,512,512,512/512,512/0/default.jpg"
)
echo "cache-miss (cold) run: cycling ${#COLD_URLS[@]} distinct level-0 tiles"
for i in "${!COLD_URLS[@]}"; do
    url="${COLD_URLS[$i]}"
    echo "  cold[$i]: $url"
    oha -z 7s -c 10 --output-format json --no-tui "$url" >"$RESULTS_DIR/cold-$i-$TIMESTAMP.json"
done

echo "done. results written to $RESULTS_DIR/{hot,cold-N}-$TIMESTAMP.json"
