#!/usr/bin/env bash
# Soak/endurance test for `ziv serve`: drives SUSTAINED mixed hot-cache-hit + cold-cache-miss
# traffic for an extended period while sampling process RSS (resident memory) and open file
# descriptor count over time, asserting both stay BOUNDED (no monotonic growth = no leak).
#
# This is a MANUAL/OPTIONAL script -- not a `cargo test` -- it needs a running server and real
# wall-clock time (minutes to hours), neither of which belong in the normal test suite. It targets
# the goals P7's plan calls out explicitly: catching blocking-thread-pool saturation and
# connection/file-descriptor leaks that a short (30s) load-test run (`scripts/load.sh`) is too
# brief to expose -- a leak of a few bytes/FDs per request is invisible in 30s but compounds over
# an hour-plus run into a visible upward trend.
#
# Usage:
#   ./scripts/soak.sh                  # quick soak, 10 minutes (default)
#   ./scripts/soak.sh 3600              # 1-hour soak
#   ./scripts/soak.sh 3600 20           # 1-hour soak, sample every 20s (default: 5s)
#
#   # 1-hour soak against a REAL remote store, with cold traffic fanned across z-planes:
#   ZIV_SOAK_SRC=https://host/img.ome.zarr \
#   ZIV_SOAK_IDENTIFIERS='@z=10,@z=40,@z=90,@z=150' ./scripts/soak.sh 3600 20
#
# A REAL soak (the kind worth trusting before a production release) runs for HOURS against a
# REMOTE store (s3://, gs://, az://, http://) -- not the local filesystem fixture this script
# defaults to -- since the interesting failure modes (connection-pool exhaustion, remote-client
# FD leaks, retry-loop accumulation) only manifest against a real network client. Point this
# script at a remote store by exporting ZIV_SOAK_SRC (see below); the default is the local
# multi-tile fixture, which still exercises the blocking-render-pool + tile-cache path end to end,
# just not the remote-store client lifecycle.
#
# What "bounded" means (how to read the output):
#   - RSS should PLATEAU after an initial warm-up climb (the tile cache filling up to its
#     byte-bound cap, plus normal allocator/OS-page steady-state growth) -- NOT climb linearly or
#     unboundedly for the rest of the run. A healthy run's RSS samples look like a curve that
#     flattens out; a leak looks like a straight line trending up with no flattening.
#   - Open FD count should be STABLE (a small, bounded band -- listening socket + per-connection
#     sockets + any open remote-store connections) -- NOT grow across the run. A leak looks like
#     FD count increasing roughly linearly with request count / elapsed time and never coming back
#     down.
# This script does not itself judge pass/fail (a hard numeric threshold would be fixture- and
# machine-specific) -- it writes every sample to a results file for you to eyeball or plot; a
# flat/plateaued RSS and FD column is a pass, a monotonically climbing one is a leak worth
# investigating.
set -euo pipefail

if ! command -v curl >/dev/null 2>&1; then
    echo "error: curl not found on PATH (required to drive traffic)." >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DURATION_SECS="${1:-600}"       # default: 10-minute quick soak
SAMPLE_INTERVAL_SECS="${2:-5}"  # default: sample every 5s
SRC="${ZIV_SOAK_SRC:-$REPO_ROOT/tests/fixtures/sample_multi_tile.ome.zarr}"
PORT="3098"
ADDR="127.0.0.1:$PORT"
BASE_URL="http://$ADDR"
RESULTS_DIR="$REPO_ROOT/bench/results"
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS_FILE="$RESULTS_DIR/soak-$TIMESTAMP.txt"

if [ "$SRC" = "$REPO_ROOT/tests/fixtures/sample_multi_tile.ome.zarr" ] && [ ! -d "$SRC" ]; then
    echo "error: fixture not found at $SRC" >&2
    echo "generate it with: cargo test -p ziv-exporter --test build_multi_tile_fixture" >&2
    exit 1
fi

mkdir -p "$RESULTS_DIR"

echo "building ziv (release) ..."
cargo build --release -p ziv --manifest-path "$REPO_ROOT/Cargo.toml"

echo "starting ziv serve on $ADDR against $SRC ..."
"$REPO_ROOT/target/release/ziv" serve "$SRC" --addr "$ADDR" &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true' EXIT

# Wait for the server to accept connections. The window is generous (120s) because startup cost
# is dominated by ZarrImage::open, which for a REMOTE store fetches and parses .zattrs/.zarray for
# every pyramid level over the network before the listener is bound -- routinely 10-30s against a
# public store, versus milliseconds for a local fixture. A 5s window silently made this script
# local-fixture-only.
READY_TIMEOUT_SECS=120
for _ in $(seq 1 "$READY_TIMEOUT_SECS"); do
    if curl -fs --max-time 10 "$BASE_URL/iiif/default/info.json" >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "error: server process exited during startup; check the store spec: $SRC" >&2
        exit 1
    fi
    sleep 1
done
if ! curl -fs --max-time 10 "$BASE_URL/iiif/default/info.json" >/dev/null 2>&1; then
    echo "error: server did not become ready within ${READY_TIMEOUT_SECS}s" >&2
    exit 1
fi

# Mixed hot/cold URL set: HOT_URL is requested every iteration (a cache hit after the first);
# COLD_URLS cycle through distinct level-0 tiles (a cache miss each time, exercising a fresh
# spawn_blocking render + remote/local read every hit) -- mirrors scripts/load.sh's hot/cold
# split, but interleaved continuously for the soak's whole duration instead of run as separate
# phases.
#
# The URL set is DERIVED from the served info.json rather than hardcoded to 512px tiles, because
# the interesting remote stores are not necessarily large in XY. Public OME-Zarr microscopy (IDR,
# BioImage Archive) is typically only a few hundred pixels wide but hundreds of planes deep, so a
# hardcoded `0,0,512,512` region would fall outside the image and return 400 for every request --
# a "soak" that never renders anything.
#
# Cold traffic: when ZIV_SOAK_IDENTIFIERS is set (comma-separated IIIF identifiers, e.g.
# '@z=10,@z=40,@z=90'), each identifier is a DISTINCT cache key over the same region, so every
# request is a genuine miss that must fetch and decompress fresh chunks from the store. Against a
# remote store that is exactly what this soak is for -- it drives the remote-client lifecycle
# (connection pool, FDs, retry paths) that a repeated hot tile never touches. Falling back to a
# spread of sub-regions (the local-fixture default) keeps the old behaviour for a large 2D image.
INFO_JSON="$(curl -fs --max-time 30 "$BASE_URL/iiif/default/info.json")"
extract_int() { echo "$INFO_JSON" | tr ',' '\n' | grep -m1 "\"$1\":" | grep -o '[0-9]\+' | head -1; }
IMG_W="$(extract_int width)"
IMG_H="$(extract_int height)"
if [ -z "$IMG_W" ] || [ -z "$IMG_H" ]; then
    echo "error: could not read width/height from info.json at $BASE_URL" >&2
    exit 1
fi
echo "image is ${IMG_W}x${IMG_H}"

FULL="0,0,$IMG_W,$IMG_H/$IMG_W,$IMG_H/0/default.jpg"
HOT_URL="$BASE_URL/iiif/default/$FULL"
COLD_URLS=()
if [ -n "${ZIV_SOAK_IDENTIFIERS:-}" ]; then
    IFS=',' read -r -a _ids <<<"$ZIV_SOAK_IDENTIFIERS"
    for id in "${_ids[@]}"; do
        COLD_URLS+=("$BASE_URL/iiif/$id/$FULL")
    done
else
    # Quarter-image sub-regions: distinct cache keys, always inside the image whatever its size.
    HW=$((IMG_W / 2)); HH=$((IMG_H / 2))
    COLD_URLS=(
        "$BASE_URL/iiif/default/0,0,$HW,$HH/$HW,$HH/0/default.jpg"
        "$BASE_URL/iiif/default/$HW,0,$HW,$HH/$HW,$HH/0/default.jpg"
        "$BASE_URL/iiif/default/0,$HH,$HW,$HH/$HW,$HH/0/default.jpg"
        "$BASE_URL/iiif/default/$HW,$HH,$HW,$HH/$HW,$HH/0/default.jpg"
    )
fi

# Fires a small burst of background curl requests (mixed hot+cold) without blocking the sampler
# loop -- this is the "sustained mixed traffic" driver. If `oha` is installed, prefer it for a
# heavier/steadier load per burst (`oha -z <interval>s -c 10 --no-tui <url>`); otherwise fall back
# to a handful of parallel curls, which needs no extra dependency.
#
# Waits on each backgrounded curl's PID EXPLICITLY (`wait "$pid"` per job) rather than a bare
# `wait` with no arguments -- a bare `wait` waits on the shell's entire background job table,
# which in some sandboxed/nested-process-group execution environments (observed in this repo's
# own dev/CI sandbox) can block indefinitely even after every actual child has exited, if the
# shell's job-control bookkeeping gets confused by how the wrapping harness manages process
# groups. Collecting PIDs and waiting on each individually avoids relying on that bookkeeping.
fire_traffic_burst() {
    if command -v oha >/dev/null 2>&1; then
        # Split the interval across the hot URL and EVERY cold URL. Driving only the hot URL
        # (as this did originally) makes the whole run a tile-cache-hit benchmark: after the
        # first request nothing reaches the store, so the remote-client lifecycle this soak
        # exists to exercise -- connection pool, FDs, retry paths -- is never touched at all.
        local n=$(( 1 + ${#COLD_URLS[@]} ))
        local slice=$(( SAMPLE_INTERVAL_SECS / n ))
        [ "$slice" -lt 1 ] && slice=1
        oha -z "${slice}s" -c 10 --no-tui "$HOT_URL" >/dev/null 2>&1 || true
        for url in "${COLD_URLS[@]}"; do
            oha -z "${slice}s" -c 4 --no-tui "$url" >/dev/null 2>&1 || true
        done
        return
    fi
    local pids=()
    for _ in 1 2 3; do
        curl -fs -o /dev/null "$HOT_URL" &
        pids+=("$!")
    done
    for url in "${COLD_URLS[@]}"; do
        curl -fs -o /dev/null "$url" &
        pids+=("$!")
    done
    for pid in "${pids[@]}"; do
        wait "$pid" 2>/dev/null || true
    done
}

# RSS (KB) via `ps`; open FD count via `lsof -p <pid>` (macOS/Linux both support `-p`; `lsof`'s
# output includes a header line, hence `tail -n +2`). Both are best-effort -- if a sample fails
# (e.g. the process just exited), record 0 rather than aborting the whole soak. NOTE: `cmd | pipe
# || echo 0` does NOT reliably fall back to 0 here -- a pipeline's exit status is the LAST
# command's (`tr`/`wc`), which succeeds even when the FIRST command (`ps`/`lsof`) found no such
# process and printed nothing, so the `||` branch never fires and an empty string / stray extra
# line can result instead. Capture into a variable first and explicitly default it if empty.
sample_rss_kb() {
    local rss
    rss="$(ps -o rss= -p "$SERVER_PID" 2>/dev/null | tr -d ' ')"
    echo "${rss:-0}"
}
sample_fd_count() {
    local fds
    fds="$(lsof -p "$SERVER_PID" 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')"
    echo "${fds:-0}"
}

{
    echo "# ziv soak run"
    echo "# started: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "# src: $SRC"
    echo "# duration_secs: $DURATION_SECS  sample_interval_secs: $SAMPLE_INTERVAL_SECS"
    echo "# image: ${IMG_W}x${IMG_H}  cold_identifiers: ${ZIV_SOAK_IDENTIFIERS:-<sub-regions>}"
    echo "# pid: $SERVER_PID"
    echo "# columns: elapsed_secs rss_kb fd_count"
} >"$RESULTS_FILE"

echo "soaking for ${DURATION_SECS}s (sampling every ${SAMPLE_INTERVAL_SECS}s) -> $RESULTS_FILE"
START_TS=$(date +%s)
END_TS=$((START_TS + DURATION_SECS))
while [ "$(date +%s)" -lt "$END_TS" ]; do
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "error: server process exited unexpectedly during the soak" >&2
        exit 1
    fi
    fire_traffic_burst

    ELAPSED=$(( $(date +%s) - START_TS ))
    RSS_KB="$(sample_rss_kb)"
    FD_COUNT="$(sample_fd_count)"
    echo "$ELAPSED $RSS_KB $FD_COUNT" | tee -a "$RESULTS_FILE"

    sleep "$SAMPLE_INTERVAL_SECS"
done

echo "done. samples written to $RESULTS_FILE"
echo "eyeball (or plot) the rss_kb and fd_count columns: both should PLATEAU, not climb"
echo "  quick check: tail -n 5 vs a middle slice -- rss_kb/fd_count should be in the same"
echo "  ballpark, not several times larger, once past the initial cache-warm-up window."
