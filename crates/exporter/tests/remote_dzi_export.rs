//! Regression test for the `--dzi` export path against a real remote store.
//!
//! `crates/exporter/src/dzi.rs`'s `export_dzi` has the identical bug shape `remote_http_export.rs`
//! covers for the IIIF path (`writer::write_tree`): both render tiles with `rayon`'s `.par_iter()`,
//! and both are bridged into the ambient tokio runtime by the same `crate::ambient_runtime_handle`
//! (see its doc comment). A grep across the whole workspace confirms these two are the exporter's
//! (and the workspace's) ONLY `rayon`-parallel sections -- `rayon`/`par_iter` appears nowhere else
//! in any crate here -- so covering both closes the pattern, not just the one line the original
//! bug report happened to name.
//!
//! Before this test, `export_dzi` was fixed alongside `write_tree` but had no dedicated
//! remote-backed regression test of its own; this is that test. Modeled on
//! `remote_http_export.rs`: same committed fixture, same loopback HTTP server on an OS-assigned
//! port (never a hardcoded one), driven through the same `tokio::task::spawn_blocking` shape
//! `crates/cli/src/lib.rs`'s `Command::Export`'s `--dzi` arm uses for `export_dzi` specifically
//! (its own, separate `spawn_blocking` call, after the IIIF export's).
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use exporter::dzi::{enumerate_dzi_tiles, export_dzi};
use iiif::Size;
use tiling::{TileEngine, ZarrTileEngine};
use zarr_core::ZarrImage;

/// Reads (and discards) one HTTP/1.1 request up to its header/body boundary, returning its path.
/// Duplicated from `remote_http_export.rs` (integration test binaries are separate compilation
/// units in this workspace, so small helpers like this are kept local rather than shared --
/// see e.g. `jpeg_pixel_dimensions` below, which is duplicated the same way in three other files).
fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let request_line = text.lines().next()?;
    let raw_path = request_line.split_whitespace().nth(1)?;
    Some(raw_path.split('?').next().unwrap_or(raw_path).to_string())
}

/// Serves whatever `root` holds, verbatim, over one connection: the committed OME-Zarr fixture
/// directory becomes the entire store, with no sabotage of any kind.
fn handle_connection(mut stream: TcpStream, root: &Path) {
    let Some(path) = read_request_path(&mut stream) else {
        return;
    };
    let rel = path.trim_start_matches('/');
    match std::fs::read(root.join(rel)) {
        Ok(bytes) => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&bytes);
        }
        Err(_) => {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    }
    let _ = stream.flush();
}

/// Spawns a background HTTP/1.1 server on loopback that serves `root` verbatim. Binds port 0 and
/// returns whatever port the OS actually assigned -- a hardcoded port would collide with
/// concurrent runs of this same test binary (see
/// `shutdown_panic.rs::reserve_ephemeral_port`'s doc comment for the exact failure mode).
fn spawn_static_http_server(root: PathBuf) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock http server");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let root = root.clone();
            std::thread::spawn(move || handle_connection(stream, &root));
        }
    });
    port
}

/// The `(width, height)` a JPEG file actually decodes to, read from its own SOF0/SOF2 marker.
/// Duplicated from `views_export.rs` (see that copy's doc comment for the full reasoning): no
/// JPEG *decoder* is a dependency anywhere in this workspace, so this reads just the header bytes
/// that carry the real, decoded pixel dimensions rather than pulling one in for a check that never
/// needs actual pixels.
fn jpeg_pixel_dimensions(bytes: &[u8]) -> (u64, u64) {
    assert_eq!(
        &bytes[0..2],
        [0xFF, 0xD8],
        "not a JPEG (missing SOI marker)"
    );
    let mut i = 2;
    while i + 4 <= bytes.len() {
        assert_eq!(
            bytes[i], 0xFF,
            "malformed JPEG: expected a marker at offset {i}"
        );
        let marker = bytes[i + 1];
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        let is_sof = matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF);
        if is_sof {
            let height = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u64;
            let width = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u64;
            return (width, height);
        }
        i += 2 + seg_len;
    }
    panic!("no SOF marker found in JPEG");
}

/// Extracts an integer XML attribute value (`name="123"`) from a `.dzi` descriptor. Enough of a
/// parse to prove the descriptor is well-formed and correct without pulling in an XML crate --
/// this workspace has none as a direct dependency, and `dzi_descriptor_xml` (`src/dzi.rs`)
/// produces a fixed, minimal shape that never needs a general parser to check.
fn parse_dzi_attr(xml: &str, name: &str) -> u64 {
    let needle = format!(r#"{name}=""#);
    let start = xml
        .find(&needle)
        .unwrap_or_else(|| panic!("{name:?} attribute missing from .dzi descriptor: {xml}"))
        + needle.len();
    let rest = &xml[start..];
    let end = rest
        .find('"')
        .unwrap_or_else(|| panic!("unterminated {name:?} attribute in .dzi descriptor: {xml}"));
    rest[..end]
        .parse()
        .unwrap_or_else(|e| panic!("{name:?} attribute is not an integer: {e} (xml: {xml})"))
}

/// Exports `sample_multi_tile.ome.zarr` as a DZI tree from a REAL http:// store on loopback,
/// driven through the same `spawn_blocking` shape `ziv export --dzi` itself uses, and checks the
/// result is actually correct rather than merely present: the `.dzi` descriptor parses and its
/// dimensions match the source image, every enumerated tile exists on disk, and a sampled tile
/// (the finest level's `col=1,row=0` cell, a real grid cell rather than a single-tile edge case)
/// decodes as a real JPEG whose own pixel dimensions match what that tile was enumerated to be.
#[tokio::test(flavor = "multi_thread")]
async fn dzi_export_from_a_real_http_store_produces_correct_output() {
    let fixture = std::fs::canonicalize("../../tests/fixtures/sample_multi_tile.ome.zarr")
        .expect("fixture directory must exist");
    let port = spawn_static_http_server(fixture);
    let url = format!("http://127.0.0.1:{port}/");

    // Metadata open: mirrors `Command::Export`'s first `spawn_blocking` call.
    let open_url = url.clone();
    let image = tokio::task::spawn_blocking(move || ZarrImage::open_with_options(&open_url, true))
        .await
        .expect("open task must not panic")
        .expect("opening the remote store must succeed");
    let engine = ZarrTileEngine::new(image);
    let (width, height) = {
        let info = engine.image_info(".");
        (info.width, info.height)
    };
    assert_eq!(
        (width, height),
        (1024, 1024),
        "expected the fixture's known 1024x1024 dimensions"
    );

    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    let tile_size = 512u64;
    let quality = 85u8;
    let name = "sample".to_string();

    // The DZI export itself: mirrors `Command::Export`'s `--dzi` arm, which runs `export_dzi` in
    // its OWN separate `spawn_blocking` call (after the IIIF export's). THIS is where the panic
    // under test happens: `export_dzi`'s rayon workers have no tokio context of their own, so a
    // remote tile read's `pollster::block_on` cannot reach a reactor for the HTTP client's
    // sockets/timers.
    let name_for_export = name.clone();
    let count = tokio::task::spawn_blocking(move || {
        export_dzi(&engine, &dir_path, &name_for_export, tile_size, quality)
    })
    .await
    .expect(
        "dzi export task panicked -- this is the regression under test: rayon's tile-rendering \
         workers have no ambient tokio context, so a remote-backed tile read's \
         `pollster::block_on` call cannot reach a reactor for the HTTP client's sockets/timers",
    )
    .expect("dzi export must succeed");

    assert!(count > 0, "dzi export must have written at least one tile");

    // Correctness #1: the `.dzi` descriptor parses, and its dimensions match the source image.
    let dzi_xml = std::fs::read_to_string(dir.path().join(format!("{name}.dzi")))
        .expect(".dzi descriptor must exist");
    let dzi_width = parse_dzi_attr(&dzi_xml, "Width");
    let dzi_height = parse_dzi_attr(&dzi_xml, "Height");
    assert_eq!(
        (dzi_width, dzi_height),
        (width, height),
        ".dzi descriptor dimensions must match the source image"
    );

    // Correctness #2: completeness -- every enumerated tile exists on disk, and the reported
    // count matches the enumerated set exactly (no orphans, nothing missing).
    let files_dir_name = format!("{name}_files");
    let tiles = enumerate_dzi_tiles(width, height, tile_size);
    assert_eq!(
        tiles.len(),
        count,
        "export_dzi's reported count must match the enumerated tile set"
    );
    for t in &tiles {
        let path = dir.path().join(t.relative_path(&files_dir_name));
        assert!(path.exists(), "expected DZI tile missing: {path:?}");
    }

    // Correctness #3: a sampled tile decodes as a real JPEG whose pixel dimensions match its own
    // enumerated size -- "a file exists at that path" is not "the export is correct".
    let finest_level = tiles.iter().map(|t| t.level).max().unwrap();
    let sample = tiles
        .iter()
        .find(|t| t.level == finest_level && t.col == 1 && t.row == 0)
        .expect("expected a real multi-tile grid cell at the finest DZI level");
    let bytes = std::fs::read(dir.path().join(sample.relative_path(&files_dir_name))).unwrap();
    let decoded = jpeg_pixel_dimensions(&bytes);
    let Size::Wh(expected_w, expected_h) = sample.size else {
        panic!("DziTile::size must be Size::Wh");
    };
    assert_eq!(
        decoded,
        (expected_w, expected_h),
        "decoded JPEG tile dimensions must match its enumerated DziTile size"
    );
}
