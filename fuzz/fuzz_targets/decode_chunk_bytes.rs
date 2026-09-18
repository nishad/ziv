#![no_main]
//! Fuzz target over ziv's compressed-chunk DECODE path — the real gap
//! `fuzz_targets/decode_chunk.rs` (metadata/JSON parsing) does NOT cover.
//!
//! `data: &[u8]` is used AS the raw on-disk bytes of a Zarr v2 chunk file (`0.0`) inside a
//! minimal, real, on-disk OME-Zarr array (built once into a reused tempdir scaffold — only the
//! chunk file is rewritten per iteration, for fuzzer throughput). The bytes are then driven
//! through the REAL production path: `ZarrImage::open` (local filesystem) +
//! `read_region_f64`, exactly what a tile-server request does against a chunk read from a local
//! or remote store.
//!
//! ## The property under test
//!
//! This must NEVER panic/abort/OOM, no matter how corrupt or adversarial `data` is. A corrupt
//! "compressed" chunk must surface as `Err` (zarrs' codec decode rejecting it, or — for the
//! `.zarray` variant below — the `DECOMPRESSED_CHUNK_BYTE_CAP` guard rejecting it before
//! decompression is even attempted). `Ok` is also an acceptable outcome for stray inputs that
//! happen to decode as *something* (e.g. an empty/degenerate gzip member) — the only outcome
//! that fails the fuzz run is a panic/abort/OOM.
//!
//! Two scaffolds are exercised per input:
//! - `SMALL_DIR`: a small (16x16 u8) declared chunk shape, gzip compressor. This is the primary
//!   target — most of the fuzzer's mutation budget goes toward the DECODE-never-panics property
//!   over corrupt compressed bytes (truncated streams, corrupt headers, garbage, flipped bits).
//!   Small keeps every iteration fast and ensures the byte cap never spuriously rejects a
//!   legitimate-shaped chunk, so failures are attributable to the fuzzed bytes, not the shape.
//! - `BOMB_DIR`: a `.zarray` declaring a HUGE chunk shape (side chosen so declared decompressed
//!   size comfortably exceeds `DECOMPRESSED_CHUNK_BYTE_CAP`) with the SAME fuzzed bytes written
//!   as the chunk file. This cheaply exercises the cap itself: `check_chunk_byte_cap` must reject
//!   every touched chunk before any decompression is attempted, regardless of what the fuzzed
//!   bytes are — so this arm's read must ALWAYS return `Err` (asserted below), never `Ok` and
//!   never panic. The corpus-tested proof of this already exists
//!   (`decompression_bomb_shaped_chunk_fails_loud_not_oom` in
//!   `crates/zarr-core/tests/decode_fuzz_corpus.rs`); this arm is a cheap fuzz-time bonus check
//!   on top of the primary decode-never-panics property.
//!
//! ## Running
//! ```sh
//! cd fuzz
//! cargo +nightly fuzz run decode_chunk_bytes --sanitizer none -- -max_total_time=30
//! ```
//! See `decode_chunk.rs`'s doc comment for why `--sanitizer none` is required in this repo's dev
//! environment (an ASan/nightly-toolchain/zarrs-`inventory`-plugin-registration link failure,
//! not a defect in this crate). `cargo +nightly fuzz check` (typecheck only) is the fallback
//! proof the target compiles if a full run isn't available.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

const SMALL_SIDE: u64 = 16;
// Side chosen so declared shape * 1 byte/elem (u8) comfortably exceeds the 64 MiB
// `DECOMPRESSED_CHUNK_BYTE_CAP` guard in `crates/zarr-core/src/image.rs` (16384^2 == 256 MiB).
const BOMB_SIDE: u64 = 16384;

fn zattrs() -> &'static str {
    r#"{
  "multiscales": [{
    "version": "0.4",
    "axes": [
      {"name":"y","type":"space"}, {"name":"x","type":"space"}
    ],
    "datasets": [
      {"path":"0","coordinateTransformations":[{"type":"scale","scale":[1,1]}]}
    ]
  }]
}"#
}

fn zarray(side: u64) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{side},{side}],\"chunks\":[{side},{side}],\"dtype\":\"|u1\",\"compressor\":{{\"id\":\"gzip\",\"level\":5}},\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}"
    )
}

/// Builds a minimal on-disk Zarr v2 array tree at `dir` with the given declared chunk `side`,
/// and an initial (valid-ish, doesn't matter — immediately overwritten per-iteration) chunk file.
fn build_scaffold(dir: &Path, side: u64) {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(dir.join(".zattrs"), zattrs()).unwrap();
    let l0 = dir.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray(side)).unwrap();
    fs::write(l0.join("0.0"), []).unwrap();
}

fn small_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("ziv-fuzz-decode-small-{}", std::process::id()));
        build_scaffold(&dir, SMALL_SIDE);
        dir
    })
}

fn bomb_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("ziv-fuzz-decode-bomb-{}", std::process::id()));
        build_scaffold(&dir, BOMB_SIDE);
        dir
    })
}

fuzz_target!(|data: &[u8]| {
    // --- Primary property: corrupt compressed bytes never panic the real decode path. ---
    let dir = small_dir();
    fs::write(dir.join("0/0.0"), data).ok();
    if let Ok(img) = zarr_core::ZarrImage::open(dir.to_str().unwrap()) {
        // Ok or Err are both fine outcomes for the read itself — only a panic/abort/OOM is a bug.
        let _ = img.read_region_f64(0, 0, 0, 0, 0..SMALL_SIDE, 0..SMALL_SIDE);
    }

    // --- Bonus: the byte-cap guard must reject a huge-declared-shape chunk BEFORE decompression,
    // --- for ANY chunk bytes (fuzzed or not) — this arm's read must always be Err, never Ok, and
    // --- (like the arm above) never panic.
    let dir = bomb_dir();
    fs::write(dir.join("0/0.0"), data).ok();
    if let Ok(img) = zarr_core::ZarrImage::open(dir.to_str().unwrap()) {
        let result = img.read_region_f64(0, 0, 0, 0, 0..BOMB_SIDE, 0..BOMB_SIDE);
        assert!(
            result.is_err(),
            "decompression-bomb-shaped chunk (declared {BOMB_SIDE}x{BOMB_SIDE}) was NOT rejected \
             by the byte-cap guard for fuzzed input {data:?}"
        );
    }
});
