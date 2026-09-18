# Load test results

Results from `scripts/load.sh`, an HTTP load test of `ziv serve` using
[`oha`](https://github.com/hatoo/oha) (`cargo install oha`, or `brew install oha` -- not a Rust
test dependency, so this is a manual/optional script, not part of `cargo test`).

## Reproducing

```sh
cargo install oha   # one-time, external binary
./scripts/load.sh
```

The script:
1. Builds `ziv` in release mode.
2. Starts `ziv serve tests/fixtures/sample_multi_tile.ome.zarr --addr 127.0.0.1:3099` in the
   background.
3. Runs `oha -z 30s -c 50 --json <url>` against a single tile URL repeated for 30s at 50
   concurrent connections (the **cache-hit / hot** case: every request after the first is served
   from the in-memory tile cache).
4. Runs `oha -z 7s -c 10 --json <url>` against each of 4 distinct level-0 tile URLs in turn (the
   **cache-miss / cold** case: each URL is a genuinely different render the first time it's hit).
5. Writes one JSON report per run to this directory, named `hot-<timestamp>.json` and
   `cold-<N>-<timestamp>.json`.

## Fixture

Uses `tests/fixtures/sample_multi_tile.ome.zarr` (1024x1024, two pyramid levels, single u8
channel -- see `crates/exporter/tests/build_multi_tile_fixture.rs` for how it's generated),
**not** any of the other committed fixtures. Every other fixture in `tests/fixtures/` is
<=64x64 pixels -- smaller than a single 512px IIIF tile -- so every request against them
degenerates to the same one full-image tile and can't produce a meaningful cache-hit/cache-miss
mix or realistic per-tile render cost. `sample_multi_tile.ome.zarr` has a real 2x2 tile grid at
level 0, which is what the hot/cold URL split above actually exercises.

Regenerate the fixture (idempotent) with:

```sh
cargo test -p ziv-exporter --test build_multi_tile_fixture
```

## Exact command

```sh
ziv serve tests/fixtures/sample_multi_tile.ome.zarr --addr 127.0.0.1:3099
oha -z 30s -c 50 --json http://127.0.0.1:3099/iiif/default/0,0,512,512/512,512/0/default.jpg
```

No result files are committed here yet -- run `scripts/load.sh` locally (or in a dedicated perf
CI job) and commit the JSON output if you want a tracked baseline.

## Soak/endurance results (`scripts/soak.sh`)

`soak-<timestamp>.txt` files come from `scripts/soak.sh`: a MANUAL/OPTIONAL endurance script
(same "not a `cargo test`" rationale as `load.sh` -- it needs a running server and real
wall-clock time) that drives SUSTAINED mixed hot-cache-hit + cold-cache-miss traffic against
`ziv serve` while sampling process RSS (KB) and open file-descriptor count every
`sample_interval_secs`, writing one `elapsed_secs rss_kb fd_count` row per sample.

```sh
./scripts/soak.sh                # quick soak, 10 minutes, sampled every 5s (defaults)
./scripts/soak.sh 3600            # 1-hour soak
./scripts/soak.sh 3600 20         # 1-hour soak, sampled every 20s
ZIV_SOAK_SRC="s3://bucket/img.ome.zarr" ./scripts/soak.sh 3600   # against a REAL remote store
```

**What "bounded" means**: RSS should PLATEAU after an initial warm-up climb (the tile cache
filling toward its byte-bound cap), not climb linearly/unboundedly for the rest of the run; open
FD count should stay in a small, STABLE band, not grow across the run. The script doesn't itself
assert pass/fail (a hard numeric threshold is fixture/machine-specific) -- eyeball or plot the
`rss_kb`/`fd_count` columns; a flat/plateaued line is a pass, a monotonically climbing one is a
leak worth investigating (catching blocking-thread-pool saturation and connection/FD leaks that
a 30s `load.sh` run is too brief to expose is the whole point).

`soak-20260703T140445Z.txt` is a committed 60s SMOKE-scale proof run (against the local
`sample_multi_tile.ome.zarr` fixture, `--sample-interval 5s`) showing the expected plateau
shape: RSS starts at ~40MB (cache warm-up), settles to ~13MB, FD count holds flat at 13 for the
whole run. A REAL soak worth trusting before a production release runs for HOURS against a
REMOTE store (`ZIV_SOAK_SRC`) -- this committed run is a shape-of-the-signal demonstration, not
that real multi-hour/remote-store soak.

## Committed baselines

`BASELINE-20260909.md` is the first committed performance reference: HTTP throughput from
`scripts/load.sh` and per-render cost from `cargo bench -p ziv-tiling`, with the hardware and
toolchain recorded. Before it there was no committed baseline at all, so no regression could be
measured against anything.

Two things that had to be fixed before either script could produce a result, both latent because
neither had ever been run end to end:

- `scripts/load.sh` passed `oha --json`, a flag oha has since replaced with
  `--output-format json`. Every invocation died before writing a file.
- `scripts/soak.sh` drove only the hot URL when `oha` was installed, so after the first request
  every response was a tile-cache hit and nothing reached the store — a "remote store soak" that
  never exercised the remote store. It now splits the interval across the hot and cold URLs.

## Soak: not yet a trustworthy result

The hours-long remote-store soak the P7 plan asks for is **still open**. Two attempts, neither
usable as a pass:

1. **A full hour against the live IDR store** (`https://uk1s3.embassy.ebi.ac.uk/idr/zarr/v0.4/
   idr0062A/6001240.zarr`, cold traffic fanned across eight z-planes). File descriptors were flat
   at 20 for the entire run with no upward trend — the remote-client FD leak this soak primarily
   exists to catch is **not** present. But the run measured the wrong thing: `fire_traffic_burst`
   drove only the hot URL when `oha` was installed, so every response after the first was a
   tile-cache hit and the store was never touched again. RSS oscillated between 18 MB and 190 MB
   with an envelope that drifted upward across quarters (min 18/42/33/58 MB, max 109/68/130/190 MB).
2. **A corrected run** with the interval split across hot and cold URLs was killed by the host at
   ~18 of 60 minutes under memory pressure. Its last samples read RSS 170-185 MB with FDs still at
   20 — noticeably higher, at the same elapsed time, than the hot-path run.

**What is and is not established.** No FD leak, on either run. RSS behaviour under genuinely mixed
remote traffic is **unresolved**: the growth is non-monotonic, so it is not obviously a leak, and
the tile cache is byte-bounded at 512 MB so it cannot be the cache filling (the working set here
is nine keys of small JPEGs, well under a megabyte). Candidates worth separating before drawing a
conclusion: allocator retention of freed decompress buffers, the `reqwest`/`object_store`
connection pool, and zarrs-internal caches.

**Do not read the committed `soak-20260703T140445Z.txt` as covering this.** It is a 60-second
local-fixture smoke run, kept only as a shape-of-the-signal demonstration.
