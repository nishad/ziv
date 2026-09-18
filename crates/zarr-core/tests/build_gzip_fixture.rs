//! Builds a committed OME-Zarr 0.4 fixture with a REAL gzip-COMPRESSED chunk (single channel,
//! small), used by `decode_fuzz_corpus.rs` to exercise the actual codec decode path (blosc/zstd/
//! gzip decompression) against malformed/corrupt compressed bytes. The other committed fixtures
//! (`sample_u8.ome.zarr`, `sample_v04.ome.zarr`, ...) all use `"compressor":null` — raw,
//! uncompressed chunks — which never touch a codec's decode function at all, so they can't stand
//! in for a decompression-bomb / corrupt-compressed-header test. Mirrors `build_u8_fixture.rs`.
//! Run: `cargo test -p ziv-zarr-core --test build_gzip_fixture`. Builds into a private temp
//! directory and checks it against the committed fixture; set `ZIV_REGENERATE_FIXTURES=1` to
//! update the committed copy.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample_gzip.ome.zarr")
}

// Single channel, single level, 16x16, dtype u8, horizontal gradient (value == x), gzip-compressed.
const SIZE: u64 = 16;

const ZATTRS: &str = r#"{
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
}"#;

fn zarray() -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{s},{s}],\"chunks\":[{s},{s}],\"dtype\":\"|u1\",\"compressor\":{{\"id\":\"gzip\",\"level\":5}},\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        s = SIZE
    )
}

/// The RAW (uncompressed) chunk bytes: value(y, x) = x (horizontal gradient), one byte per pixel.
/// Exposed (not just used internally) so `decode_fuzz_corpus.rs` can reconstruct the exact
/// decompressed-size expectation without re-deriving it.
pub fn raw_chunk_bytes() -> Vec<u8> {
    let mut buf = Vec::with_capacity((SIZE * SIZE) as usize);
    for _y in 0..SIZE {
        for x in 0..SIZE {
            buf.push(x as u8);
        }
    }
    buf
}

/// Gzip-compresses `raw_chunk_bytes()` — the exact bytes a legitimate `0.0` chunk file contains
/// under this fixture's `.zarray` (`"compressor":{"id":"gzip","level":5}`).
pub fn compressed_chunk_bytes() -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(5));
    encoder.write_all(&raw_chunk_bytes()).unwrap();
    encoder.finish().unwrap()
}

fn build(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();

    let l0 = root.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray()).unwrap();
    fs::write(l0.join("0.0"), compressed_chunk_bytes()).unwrap();
}

#[test]
fn builds_fixture() {
    fixture_test_support::check_or_regenerate(
        &fixture_root(),
        "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_gzip_fixture",
        |root| {
            build(root);
            assert!(root.join(".zgroup").exists());
            assert!(root.join("0/.zarray").exists());
            // Sanity: the committed chunk bytes are smaller than the raw (compression actually
            // happened) and round-trip back to the original gradient via a real gzip decoder.
            let compressed = fs::read(root.join("0/0.0")).unwrap();
            assert!(compressed.len() < raw_chunk_bytes().len());
            let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
            let mut decoded = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut decoded).unwrap();
            assert_eq!(decoded, raw_chunk_bytes());
        },
    );
}
