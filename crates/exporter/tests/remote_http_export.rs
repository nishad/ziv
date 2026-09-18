//! Regression test for the flagship `ziv export <http(s)://...>` use case, reproduced by hand
//! against a real remote store:
//!
//! ```text
//! $ ziv export https://uk1s3.embassy.ebi.ac.uk/idr/zarr/v0.4/idr0101A/13457537.zarr ./out ...
//! ziv export: planning 24 views x 4 tiles = 96 tiles
//! thread '<unnamed>' panicked at library/core/src/ops/function.rs:250:5:
//! there is no reactor running, must be called from the context of a Tokio 1.x runtime
//! ```
//!
//! The metadata open succeeds (it prints the plan); tile rendering is what dies. `write_tree`
//! (`src/writer.rs`) renders tiles with `rayon`'s `.par_iter()`, and each tile read eventually
//! drives a remote-store future with `pollster::block_on` (`zarr_core::image`). `pollster` itself
//! needs no ambient runtime, but the underlying HTTP client's sockets/timers DO need a live tokio
//! reactor, and rayon's global thread-pool workers carry no tokio context at all — unlike `ziv
//! serve`, whose reads run inside `tokio::task::spawn_blocking`, whose worker threads DO retain
//! the runtime's context. That mismatch is exactly what this test reproduces: it drives the
//! export through the same two `spawn_blocking` calls `crates/cli/src/lib.rs`'s `Command::Export`
//! arm makes (metadata open, then the export itself), against a REAL HTTP server on loopback, so
//! the rayon workers spawned deep inside really do have zero tokio context, exactly as in
//! production.
//!
//! Every other test under this directory exports from a local fixture path, so none of them ever
//! exercised this at all (`ZarrImage::open`'s local branch never touches `pollster::block_on`).
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use exporter::{enumerate_request_space, export_with_progress, ExportOptions};
use iiif::{ProjectionId, Region, Size};
use tiling::{TileEngine, ZarrTileEngine};
use zarr_core::ZarrImage;

/// Reads (and discards) one HTTP/1.1 request up to its header/body boundary, returning its path.
/// Good enough for `object_store`'s GET-only traffic against a plain HTTP store (no chunked
/// request bodies, no trailers) -- the same minimal parsing `crates/cli/tests/shutdown_panic.rs`
/// uses for the same reason.
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
/// directory becomes the entire store, with no sabotage of any kind (unlike
/// `shutdown_panic.rs`'s mock, this test is about the ambient-runtime bug, not retry behavior).
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

/// Spawns a background HTTP/1.1 server on loopback that serves `root` (an OME-Zarr directory)
/// verbatim. Binds port 0 and returns whatever port the OS actually assigned -- a hardcoded port
/// would collide with concurrent runs of this same test binary (see
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

/// Exports `sample_multi_tile.ome.zarr` (a genuine 2x2 level-0 tile grid; see
/// `multi_tile_export.rs`'s doc comment) from a REAL http:// store on loopback, driven through the
/// exact same two `spawn_blocking` calls `ziv export` itself makes, and checks the result is
/// actually correct: every enumerated tile exists on disk, and a sampled on-disk tile's bytes
/// match a direct `engine.tile(...)` call against the same HTTP-backed engine.
#[tokio::test(flavor = "multi_thread")]
async fn export_from_a_real_http_store_produces_correct_tiles() {
    let fixture = std::fs::canonicalize("../../tests/fixtures/sample_multi_tile.ome.zarr")
        .expect("fixture directory must exist");
    let port = spawn_static_http_server(fixture);
    let url = format!("http://127.0.0.1:{port}/");

    // Metadata open: mirrors `Command::Export`'s first `spawn_blocking` call. This has ambient
    // tokio context (spawn_blocking threads retain it), so it succeeds even before any fix --
    // matching the manual repro, where the plan prints fine and only tile rendering panics.
    let open_url = url.clone();
    let image = tokio::task::spawn_blocking(move || ZarrImage::open_with_options(&open_url, true))
        .await
        .expect("open task must not panic")
        .expect("opening the remote store must succeed");
    let engine = ZarrTileEngine::new(image);

    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();

    // The export itself: mirrors `Command::Export`'s second `spawn_blocking` call, which is what
    // actually contains `write_tree`'s `rayon::par_iter()` tile-rendering loop. THIS is where the
    // panic under test happens: rayon's worker threads have no tokio context of their own, so a
    // remote tile read's `pollster::block_on` cannot reach a reactor for the HTTP client's
    // sockets/timers.
    let (summary, engine) = tokio::task::spawn_blocking(move || {
        export_with_progress(&engine, &dir_path, &ExportOptions::default(), &mut |_| {})
            .map(|summary| (summary, engine))
    })
    .await
    .expect(
        "export task panicked -- this is the regression under test: rayon's tile-rendering \
         workers have no ambient tokio context, so a remote-backed tile read's \
         `pollster::block_on` call cannot reach a reactor for the HTTP client's sockets/timers",
    )
    .expect("export must succeed");

    assert!(
        summary.tiles > 0,
        "export must have written at least one tile"
    );

    // Correctness, not just survival: every enumerated request must exist on disk...
    let info = engine.image_info(".");
    let requests = enumerate_request_space(&info);
    assert!(
        requests.len() >= 4,
        "expected the fixture's real 2x2 level-0 tile grid, got {} requests",
        requests.len()
    );
    for req in &requests {
        let path = dir.path().join(req.relative_path());
        assert!(path.exists(), "expected exported file missing: {path:?}");
    }

    // ...and a sampled on-disk tile's bytes must match a direct `engine.tile(...)` call against
    // the SAME (HTTP-backed) engine -- not merely "some file exists at that path".
    let region = Region::Px {
        x: 512,
        y: 0,
        w: 512,
        h: 512,
    };
    let size = Size::Wh(512, 512);
    let on_disk = std::fs::read(dir.path().join("512,0,512,512/512,512/0/default.jpg")).unwrap();
    let direct =
        tokio::task::spawn_blocking(move || engine.tile(&ProjectionId::Default, region, size, 85))
            .await
            .expect("direct tile read task must not panic")
            .expect("direct tile read must succeed");
    assert_eq!(
        on_disk, direct,
        "exported tile bytes must match a direct engine.tile() call over the same http store"
    );
}
