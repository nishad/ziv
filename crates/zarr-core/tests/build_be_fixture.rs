//! Builds committed big-endian-dtype OME-Zarr 0.4 test fixtures, used to prove `ziv` opens AND
//! correctly decodes big-endian Zarr V2 arrays end-to-end (see `image.rs`'s `classify_dtype` and
//! `register_one_byte_endian_aliases`).
//!
//! Three fixtures:
//!  - `sample_be_u1.ome.zarr`: dtype `>u1` (big-endian u8). Endianness is meaningless for 1-byte
//!    types, but zarrs 0.23.13 only registers the `|u1` V2 alias for `UInt8DataType` (see
//!    `uint.rs` in the zarrs source: `v2: "|u1", ["|u1"]` — no `>u1`/`<u1` aliases), so a real
//!    `>u1`-declared `.zarray` fails at `Array::open` with "data type >u1 is not supported"
//!    *before* `classify_dtype` ever runs. `ziv` must normalize the dtype string itself.
//!  - `sample_be_u2.ome.zarr`: dtype `>u2` (big-endian u16), chunk bytes written in genuine
//!    big-endian byte order (`to_be_bytes`) — proves the decode path returns byte-swapped-correct
//!    (not raw/native-endian garbage) values. zarrs DOES register both `<u2`/`>u2` as aliases for
//!    the same `UInt16DataType` (confirmed in the zarrs source), and the V2->V3 conversion threads
//!    the parsed endianness into the `bytes` codec, which byte-swaps on decode — so `>u2` already
//!    opens and reads correctly with no ziv-side change; this fixture is the proof.
//!  - `sample_be_i4.ome.zarr`: dtype `>i4` (big-endian i32), chunk bytes written in genuine
//!    big-endian byte order — a second, differently-signed/differently-sized multi-byte type,
//!    proving the "zarrs already handles multi-byte big-endian correctly" finding generalizes
//!    beyond u16 specifically (also empirically spot-checked during investigation for i2/u4/f4/f8,
//!    all of which behaved identically — see the report for the full list).
//!
//! Mirrors `build_u8_fixture.rs`/`build_fixture.rs` (no Python/zarr dependency, raw zarr v2
//! uncompressed chunks). Run: `cargo test -p ziv-zarr-core --test build_be_fixture`. Each of the
//! three fixtures below is built into its own private temp directory and checked against its
//! committed copy; set `ZIV_REGENERATE_FIXTURES=1` to update all three committed copies.
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../tests/fixtures/{name}"))
}

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

fn zarray(dtype: &str) -> String {
    format!(
        "{{\"zarr_format\":2,\"shape\":[{s},{s}],\"chunks\":[{s},{s}],\"dtype\":\"{dtype}\",\"compressor\":null,\"fill_value\":0,\"order\":\"C\",\"filters\":null,\"dimension_separator\":\".\"}}",
        s = SIZE
    )
}

/// value(y, x) = x (horizontal gradient), one byte per pixel — endianness-irrelevant.
fn chunk_bytes_u8() -> Vec<u8> {
    let mut buf = Vec::with_capacity((SIZE * SIZE) as usize);
    for _y in 0..SIZE {
        for x in 0..SIZE {
            buf.push(x as u8);
        }
    }
    buf
}

/// value(y, x) = x (horizontal gradient), two bytes per pixel, written in BIG-ENDIAN byte order
/// (`to_be_bytes`) — a genuine big-endian-authored chunk, not a relabeled little-endian one.
fn chunk_bytes_u16_be() -> Vec<u8> {
    let mut buf = Vec::with_capacity((SIZE * SIZE * 2) as usize);
    for _y in 0..SIZE {
        for x in 0..SIZE {
            buf.extend_from_slice(&(x as u16).to_be_bytes());
        }
    }
    buf
}

/// value(y, x) = x (horizontal gradient), four bytes per pixel, written in BIG-ENDIAN byte order.
fn chunk_bytes_i32_be() -> Vec<u8> {
    let mut buf = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _y in 0..SIZE {
        for x in 0..SIZE {
            buf.extend_from_slice(&(x as i32).to_be_bytes());
        }
    }
    buf
}

fn build(root: &Path, dtype: &str, chunk: Vec<u8>) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    fs::write(root.join(".zattrs"), ZATTRS).unwrap();

    let l0 = root.join("0");
    fs::create_dir_all(&l0).unwrap();
    fs::write(l0.join(".zarray"), zarray(dtype)).unwrap();
    fs::write(l0.join("0.0"), chunk).unwrap();
}

/// (fixture directory name, declared dtype string, the function that builds its one chunk).
type BeFixtureSpec = (&'static str, &'static str, fn() -> Vec<u8>);

#[test]
fn builds_fixtures() {
    let fixtures: [BeFixtureSpec; 3] = [
        ("sample_be_u1.ome.zarr", ">u1", chunk_bytes_u8),
        ("sample_be_u2.ome.zarr", ">u2", chunk_bytes_u16_be),
        ("sample_be_i4.ome.zarr", ">i4", chunk_bytes_i32_be),
    ];
    for (name, dtype, chunk_fn) in fixtures {
        fixture_test_support::check_or_regenerate(
            &fixture_root(name),
            "ZIV_REGENERATE_FIXTURES=1 cargo test -p ziv-zarr-core --test build_be_fixture",
            |root| {
                build(root, dtype, chunk_fn());
                assert!(root.join(".zgroup").exists());
                assert!(root.join("0/.zarray").exists());
                assert!(root.join("0/0.0").exists());
            },
        );
    }
}
