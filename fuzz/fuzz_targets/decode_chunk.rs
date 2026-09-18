#![no_main]
//! Fuzz target over ziv's OME-Zarr multiscales/attributes metadata parser
//! (`zarr_core::parse_multiscale`) — the JSON-shaped half of the decode surface that processes
//! untrusted bytes (a `.zattrs`/`zarr.json` blob read from local disk OR a remote object store).
//!
//! `data` is treated as arbitrary bytes that MIGHT be JSON (most fuzzer-generated inputs won't
//! parse as JSON at all — `serde_json::from_slice` rejecting them is itself a fine, fast, no-op
//! outcome for the fuzzer to explore past); when it IS valid JSON and happens to be an object,
//! it's fed straight into `parse_multiscale`, exactly as `ZarrImage::open_local`/`open_remote`
//! do with real group attributes. The property under test: `parse_multiscale` must NEVER panic,
//! regardless of how adversarial the JSON shape is (wrong types, missing fields, deeply nested
//! garbage, huge numbers, non-UTF8-adjacent strings, etc) — it must always return a `Result`.
//!
//! ## Why metadata parsing, not raw chunk bytes, is the fuzz target here
//!
//! The OTHER half of the decode surface — a compressed zarr CHUNK's bytes (blosc/gzip/zstd) —
//! is decoded by `zarrs`' own codec implementations, not code this crate owns; fuzzing arbitrary
//! bytes AS a chunk would mostly be fuzzing `zarrs`/the C blosc library, not ziv's own code, and
//! the interesting, ziv-owned property for that path (a decompression-bomb-shaped chunk must be
//! rejected before an unbounded allocation) is exactly what
//! `crates/zarr-core/tests/decode_fuzz_corpus.rs`'s `decompression_bomb_shaped_chunk_fails_loud_not_oom`
//! test proves deterministically (constructing a REAL gzip-compressed fixture, which a raw-bytes
//! libFuzzer harness can't easily do without a JSON/zarr-tree scaffold around the fuzzed bytes).
//! `parse_multiscale` is the metadata-parse decode path that's both (a) entirely ziv-owned code
//! (worth fuzzing) and (b) naturally fuzzable as `data: &[u8]` with no filesystem scaffolding.
//!
//! ## Running
//! ```sh
//! cd fuzz
//! cargo +nightly fuzz run decode_chunk --sanitizer none -- -max_total_time=30
//! ```
//! `--sanitizer none` is REQUIRED in this repo's dev environment as of this writing: the default
//! ASan-instrumented build fails at LINK time (`ld: initializer pointer has no target`) against
//! this specific nightly (2026-07-02) + macOS 26 + LLVM 22 combination — a toolchain/ASan
//! interaction with `zarrs`' `inventory`-based static codec-plugin registration (`inventory::submit!`
//! in every codec module, e.g. `gzip.rs`), not a defect in this crate's code. `cargo +nightly fuzz
//! check` (typecheck only, no ASan link) succeeds cleanly, confirming the target itself compiles;
//! `--sanitizer none` produces a working, runnable binary (sans memory-safety instrumentation) —
//! still finds panics/aborts, just not sub-byte memory-safety bugs ASan would additionally catch.
//! A recorded run reached about 2.4 million executions with no crashes across two runs.
//! Requires the `nightly` toolchain, since `cargo-fuzz` and libFuzzer need nightly's
//! instrumentation flags. Where nightly is unavailable, the corpus test above is the fallback.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let Some(obj) = value.as_object() else {
        return;
    };
    // Must never panic — Ok or Err are both fine outcomes.
    let _ = zarr_core::parse_multiscale(obj);
});
