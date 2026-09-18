//! Decoder-fuzzing corpus test (Deliverable B) — the RUNNING PROOF that malformed/adversarial
//! bytes reaching the zarr chunk-decode / metadata-parse path fail LOUD (`Result::Err`), never
//! panic-crash the process or OOM.
//!
//! `cargo-fuzz` IS available in this environment (nightly toolchain + `cargo-fuzz` binary both
//! installed — see `/Users/nishad/Projects/zarr-lab/fuzz/`), so a real `cargo fuzz run` short
//! session backs this up too (see the fuzz target's own doc comment for the exact command — a
//! `--sanitizer none` build was needed to work around an ASan/nightly-toolchain linker issue in
//! this dev environment — and the P7b report for the run transcript: ~2.4M execs, 0 crashes).
//! This corpus test is the deterministic, CI-friendly, always-runs-under-stable complement: a
//! hand-crafted set of malformed inputs run through the REAL decode path (not a mock), each
//! asserted to return `Err` without panicking. `cargo test` itself catches a panic and fails the
//! test loud, so "no panic" is enforced by the harness, not just eyeballed.
//!
//! ## Why a REAL gzip-compressed fixture (not the existing `compressor:null` ones)
//!
//! Every other committed fixture (`sample_u8.ome.zarr`, `sample_v04.ome.zarr`, ...) stores chunks
//! UNCOMPRESSED (`"compressor":null`) — reading them never calls a codec's `decode()` at all, so
//! they can't stand in for "malformed COMPRESSED bytes". This file builds its own gzip-compressed
//! (`"compressor":{"id":"gzip","level":5}`) throwaway zarr trees per test (via `tempfile`), then
//! corrupts the compressed chunk bytes in the specific ways a hostile/corrupt remote object would:
//! truncation, corrupt header, and a decompression-bomb shape. See the module doc comment on
//! `crates/zarr-core/src/image.rs`'s `retrieve_widened`/`retrieve_widened_async` for the
//! DECOMPRESSED_CHUNK_BYTE_CAP guard this test also exercises (Deliverable B, point 3).
//!
//! ## Corpus
//! 1. Truncated chunk (a handful of bytes chopped off a valid gzip stream).
//! 2. Corrupt compressed header (gzip magic bytes overwritten).
//! 3. Empty chunk file (zero bytes).
//! 4. Lying-shape metadata (`.zarray` `"shape"`/`"chunks"` inflated far beyond the real chunk
//!    file's content, without touching MAX_READ_PIXELS-scale numbers — a plain "chunk file is
//!    smaller than the shape claims" mismatch).
//! 5. A decompression-bomb-shaped chunk: a tiny, HIGHLY compressible gzip stream (all zero bytes)
//!    whose UNCOMPRESSED size is declared/expected to be enormous relative to its compressed
//!    size, exercising the `DECOMPRESSED_CHUNK_BYTE_CAP` guard added in `image.rs`.
//! 6. Malformed OME-Zarr attributes JSON (garbage bytes in `.zattrs`, and truncated/invalid JSON)
//!    fed through `parse_multiscale`/`ZarrImage::open` directly (the metadata-parse half of
//!    Deliverable B, independent of chunk decode).
//! 7. A further hand-crafted corpus of corrupt compressed-chunk byte-strings (empty, truncated
//!    header, truncated body, corrupted CRC trailer, all-zero, all-0xff, plain garbage) — see
//!    `corrupt_compressed_chunk_bytes_never_panic_and_fail_loud` below. This is the CI-runnable
//!    (stable-toolchain) complement to the 2nd libFuzzer target,
//!    `fuzz/fuzz_targets/decode_chunk_bytes.rs`, which fuzzes the SAME real scaffold-and-read
//!    path over randomly-mutated bytes (the fuzz crate is its own workspace, so it never runs
//!    under `cargo test --workspace` — this test is what actually runs in CI).
use std::fs;
use std::io::Write;
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;
use zarr_core::{parse_multiscale, ZarrError, ZarrImage};

const SIZE: u64 = 16;

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
  }],
  "omero": { "channels": [
    {"color":"FFFFFF","window":{"start":0,"end":15},"active":true}
  ]}
}"#
}

fn zarray(shape: u64, chunks: u64) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{shape},{shape}],\"chunks\":[{chunks},{chunks}],\"dtype\":\"|u1\",\"compressor\":{{\"id\":\"gzip\",\"level\":5}},\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}"
    )
}

fn raw_chunk_bytes() -> Vec<u8> {
    let mut buf = Vec::with_capacity((SIZE * SIZE) as usize);
    for _y in 0..SIZE {
        for x in 0..SIZE {
            buf.push(x as u8);
        }
    }
    buf
}

fn valid_compressed_chunk() -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(5));
    encoder.write_all(&raw_chunk_bytes()).unwrap();
    encoder.finish().unwrap()
}

/// Builds a throwaway single-level gzip-compressed OME-Zarr 0.4 tree under `dir`, with the given
/// `.zarray` shape/chunks and raw chunk-file bytes (already "compressed" or corrupted — the
/// caller controls exactly what ends up on disk at `0/0.0`).
fn build_tree(dir: &Path, shape: u64, chunks: u64, chunk_bytes: &[u8]) {
    fs::write(dir.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(dir.join(".zattrs"), zattrs()).unwrap();
    let l0 = dir.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray(shape, chunks)).unwrap();
    fs::write(l0.join("0.0"), chunk_bytes).unwrap();
}

/// Attempts a full open + region read against the tree at `dir`; returns whatever `Result` comes
/// back (the caller asserts `Err`) — the ACT of calling this without the process panicking is
/// itself part of what's being proven (a `#[test]` fn that panics fails loud via the test
/// harness, so "didn't panic" is enforced automatically by `cargo test`'s own pass/fail logic).
fn open_and_read(dir: &Path) -> Result<(), ZarrError> {
    let img = ZarrImage::open(dir.to_str().unwrap())?;
    img.read_region_f64(0, 0, 0, 0, 0..SIZE, 0..SIZE)?;
    Ok(())
}

// --- Sanity: the well-formed baseline must actually work (proves the corpus below is testing
// --- REAL malformation, not just "this whole codec path is broken"). ---

#[test]
fn sanity_valid_gzip_chunk_reads_correctly() {
    let dir = tempfile::tempdir().unwrap();
    build_tree(dir.path(), SIZE, SIZE, &valid_compressed_chunk());
    let img = ZarrImage::open(dir.path().to_str().unwrap()).unwrap();
    let tile = img.read_region_f64(0, 0, 0, 0, 0..SIZE, 0..SIZE).unwrap();
    assert_eq!(tile[[3, 7]], 7.0);
    assert_eq!(tile[[0, 15]], 15.0);
}

// --- Malformed corpus: each must fail LOUD (Err), never panic. ---

#[test]
fn truncated_gzip_chunk_fails_loud() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = valid_compressed_chunk();
    bytes.truncate(bytes.len() / 2); // chop the stream in half mid-deflate-block
    build_tree(dir.path(), SIZE, SIZE, &bytes);
    assert!(open_and_read(dir.path()).is_err());
}

#[test]
fn truncated_to_a_handful_of_bytes_fails_loud() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = valid_compressed_chunk();
    bytes.truncate(3); // not even a full gzip header (10 bytes minimum)
    build_tree(dir.path(), SIZE, SIZE, &bytes);
    assert!(open_and_read(dir.path()).is_err());
}

#[test]
fn empty_chunk_file_fails_loud() {
    let dir = tempfile::tempdir().unwrap();
    build_tree(dir.path(), SIZE, SIZE, &[]);
    assert!(open_and_read(dir.path()).is_err());
}

#[test]
fn corrupt_gzip_magic_bytes_fail_loud() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = valid_compressed_chunk();
    // Gzip magic is 0x1f 0x8b; corrupt it so the stream isn't recognized as gzip at all.
    bytes[0] = 0x00;
    bytes[1] = 0x00;
    build_tree(dir.path(), SIZE, SIZE, &bytes);
    assert!(open_and_read(dir.path()).is_err());
}

#[test]
fn corrupt_middle_bytes_fail_loud_or_produce_no_panic() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = valid_compressed_chunk();
    // Flip bytes in the middle of the compressed stream (past the header) — this may surface as
    // a decode error (corrupt deflate stream / bad CRC) OR, in the worst case for a weak codec,
    // silently decode to garbage; either is acceptable AS LONG AS IT DOESN'T PANIC. If it
    // "succeeds" the subsequent reshape either matches (fine) or the values are simply wrong
    // (not a safety issue — no OOM/panic), so this test only asserts no panic, not always-Err.
    let mid = bytes.len() / 2;
    for b in bytes.iter_mut().skip(mid).take(4) {
        *b ^= 0xff;
    }
    build_tree(dir.path(), SIZE, SIZE, &bytes);
    let _ = open_and_read(dir.path()); // must not panic; Ok or Err both acceptable here
}

#[test]
fn lying_shape_metadata_fails_loud() {
    // `.zarray` claims a MUCH larger shape/chunk than the actual (small, real) chunk file
    // contains — the decoded byte count won't match the declared element count.
    let dir = tempfile::tempdir().unwrap();
    build_tree(dir.path(), 4096, 4096, &valid_compressed_chunk());
    let result = ZarrImage::open(dir.path().to_str().unwrap())
        .and_then(|img| img.read_region_f64(0, 0, 0, 0, 0..4096, 0..4096));
    assert!(result.is_err());
}

#[test]
fn lying_shape_smaller_than_chunk_still_fails_loud_or_reads_partial() {
    // The inverse: `.zarray` claims a SMALLER shape than the chunk's own declared chunk size
    // (chunks > shape) — a degenerate but not impossible malformed-metadata shape. Must not
    // panic regardless of whether zarrs accepts or rejects opening it.
    let dir = tempfile::tempdir().unwrap();
    build_tree(dir.path(), 4, 16, &valid_compressed_chunk());
    let _ = open_and_read(dir.path()); // must not panic
}

/// Decompression-bomb shape: a tiny, maximally-compressible payload (all zero bytes — gzip
/// compresses these to a handful of bytes) declared as a chunk whose shape EXACTLY matches the
/// bomb's true (huge) decompressed size — deliberately an exact match, not a mismatch, so this
/// test proves the `DECOMPRESSED_CHUNK_BYTE_CAP` guard itself rejects it BEFORE decompression,
/// rather than accidentally passing because of an unrelated shape-mismatch error (an earlier,
/// buggy version of this test used a non-exact `sqrt`-derived side and passed even with NO
/// guard present, because the decode "succeeded" — fully decompressing the whole bomb into
/// memory — and only failed afterward on an unrelated element-count mismatch; that masked the
/// real vulnerability. `side=16384` makes shape*shape*sizeof(u8) exactly 256 MiB — comfortably
/// above `DECOMPRESSED_CHUNK_BYTE_CAP`'s 64 MiB cap — so with an exact shape match the ONLY
/// thing that can reject this read is the byte-cap guard itself).
///
/// 256 MiB stays a genuinely fast test despite the large declared size: the COMPRESSED bytes
/// written to disk are tiny (all zeros compress to a few KiB even at max gzip level), and the
/// guard rejects the chunk BEFORE any decompression is attempted — this test is exactly what
/// proves that "reject before decompressing" property; if the guard were missing or broken (as
/// in the pre-fix version of this test), zarrs would actually attempt to allocate/decompress the
/// full 256 MiB, which is the real-world danger this guard closes.
#[test]
fn decompression_bomb_shaped_chunk_fails_loud_not_oom() {
    let dir = tempfile::tempdir().unwrap();
    const SIDE: u64 = 16384;
    const BOMB_SIZE: u64 = SIDE * SIDE; // == 256 MiB for u8, and EXACTLY matches the .zarray shape
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&vec![0u8; BOMB_SIZE as usize]).unwrap();
    let bomb_bytes = encoder.finish().unwrap();
    // Compression ratio proof: the compressed bomb is tiny relative to its declared/decompressed
    // size — the hallmark of a decompression bomb (a legitimate photo/microscopy chunk doesn't
    // compress 1000:1).
    assert!(bomb_bytes.len() < (BOMB_SIZE as usize) / 100);

    build_tree(dir.path(), SIDE, SIDE, &bomb_bytes);
    let result = ZarrImage::open(dir.path().to_str().unwrap())
        .and_then(|img| img.read_region_f64(0, 0, 0, 0, 0..SIDE, 0..SIDE));
    assert!(
        result.is_err(),
        "expected the decompression-bomb-shaped chunk (exact shape match, no mismatch to \
         accidentally reject it) to be rejected by the byte-cap guard"
    );
}

// --- Metadata-parse half of Deliverable B: malformed `.zattrs` / multiscales JSON. ---

#[test]
fn garbage_bytes_in_zattrs_fail_loud() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(
        dir.path().join(".zattrs"),
        b"\x00\x01\xff\xfe not json at all {{{",
    )
    .unwrap();
    let l0 = dir.path().join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray(SIZE, SIZE)).unwrap();
    fs::write(l0.join("0.0"), valid_compressed_chunk()).unwrap();
    assert!(ZarrImage::open(dir.path().to_str().unwrap()).is_err());
}

#[test]
fn truncated_json_in_zattrs_fails_loud() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    let truncated = &zattrs()[..zattrs().len() / 3];
    fs::write(dir.path().join(".zattrs"), truncated).unwrap();
    let l0 = dir.path().join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray(SIZE, SIZE)).unwrap();
    fs::write(l0.join("0.0"), valid_compressed_chunk()).unwrap();
    assert!(ZarrImage::open(dir.path().to_str().unwrap()).is_err());
}

// --- CI-runnable (stable toolchain) complement to `fuzz/fuzz_targets/decode_chunk_bytes.rs` ---
//
// The fuzz crate is its own workspace (see `fuzz/Cargo.toml`'s doc comment) and needs nightly +
// libFuzzer, so it never runs under plain `cargo test --workspace`. This block is the
// deterministic, always-runs-under-stable proof of the SAME property that fuzz target hunts for:
// a handful of hand-crafted CORRUPT compressed-chunk byte-strings, run through the real
// scaffold-and-read path (`ZarrImage::open` + `read_region_f64` against a real on-disk gzip-
// compressed Zarr v2 array), must each return `Err` without panicking.

/// Feeds `chunk_bytes` in as the raw `0.0` chunk file of a small (16x16 u8, gzip-compressor)
/// real on-disk Zarr v2 array, then opens + reads it — returning whatever `Result` comes back.
/// A panic here fails the test via `cargo test`'s own harness (not an explicit assertion), so
/// "didn't panic" is enforced by the test runner itself, exactly like `open_and_read` above.
fn open_and_read_with_chunk_bytes(chunk_bytes: &[u8]) -> Result<(), ZarrError> {
    let dir = tempfile::tempdir().unwrap();
    build_tree(dir.path(), SIZE, SIZE, chunk_bytes);
    open_and_read(dir.path())
}

/// Corrupt/adversarial compressed-chunk byte-strings a fuzzer would plausibly stumble across —
/// each driven through the REAL decode path and asserted to fail loud (`Err`), never panic.
#[test]
fn corrupt_compressed_chunk_bytes_never_panic_and_fail_loud() {
    let valid = valid_compressed_chunk();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        ("single byte", vec![0x1f]),
        (
            "garbage, not gzip at all",
            b"not a gzip stream at all, just garbage bytes".to_vec(),
        ),
        ("all zero bytes, gzip-header-length", vec![0u8; 10]),
        ("all 0xff bytes", vec![0xffu8; 32]),
        ("valid gzip header, truncated body", {
            let mut b = valid.clone();
            b.truncate(12); // past the 10-byte gzip header, but no deflate body/trailer
            b
        }),
        ("valid gzip, truncated to half", {
            let mut b = valid.clone();
            b.truncate(b.len() / 2);
            b
        }),
        ("valid gzip, corrupted CRC trailer", {
            let mut b = valid.clone();
            let len = b.len();
            for byte in b.iter_mut().skip(len.saturating_sub(4)) {
                *byte ^= 0xff;
            }
            b
        }),
    ];
    for (label, bytes) in cases {
        let result = open_and_read_with_chunk_bytes(&bytes);
        assert!(
            result.is_err(),
            "case {label:?} ({bytes:?}) unexpectedly succeeded; expected a decode Err"
        );
    }
}

/// Fuzzes `parse_multiscale` directly (the pure JSON-parsing half, independent of any filesystem
/// tree) over a small set of adversarial JSON shapes — never panics, always returns `Result`.
/// This is the exact function the module doc comment on `fuzz/fuzz_targets/decode_chunk.rs`
/// names as the metadata-parse fuzz surface; this loop is the deterministic corpus proof of the
/// same property `cargo fuzz run` explores randomly.
#[test]
fn parse_multiscale_never_panics_on_adversarial_json() {
    let adversarial_inputs = [
        "{}",
        r#"{"multiscales": null}"#,
        r#"{"multiscales": "not an array"}"#,
        r#"{"multiscales": []}"#,
        r#"{"multiscales": [{}]}"#,
        r#"{"multiscales": [{"axes": null}]}"#,
        r#"{"multiscales": [{"axes": [], "datasets": []}]}"#,
        r#"{"multiscales": [{"axes": [{"name": 123}], "datasets": [{"path":"0"}]}]}"#,
        r#"{"multiscales": [{"axes": [{"name": "y"}], "datasets": "nope"}]}"#,
        r#"{"ome": {"multiscales": [{"axes": [{"name": " "}], "datasets": [{"path":"0"}]}]}}"#,
        r#"{"ome": {"version": 999, "multiscales": [{"axes": [], "datasets": []}]}}"#,
        r#"{"multiscales": [{"version": "99.99", "axes": [{"name":"y","type":"space"}], "datasets": [{"path":"0"}]}]}"#,
    ];
    for input in adversarial_inputs {
        let value: serde_json::Value = match serde_json::from_str(input) {
            Ok(v) => v,
            Err(_) => continue, // not even valid JSON syntax; parse_multiscale isn't reached
        };
        let Some(obj) = value.as_object() else {
            continue;
        };
        // The call itself must not panic — `cargo test` fails loud if it does. We don't assert
        // Err/Ok uniformly (a couple of these ARE valid-ish and may parse Ok), only that calling
        // it is safe.
        let _ = parse_multiscale(obj);
    }
}
