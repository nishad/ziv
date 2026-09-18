//! Signal-injection reproduction of the shutdown-time panic first seen in a
//! one-hour remote soak (see `docs/operations.md`): a worker thread panicking with "A Tokio 1.x context was found, but it is
//! being shutdown" when SIGTERM arrives while a remote chunk read is mid-retry.
//!
//! This is the real mechanism, not a synthetic panic: it spawns the REAL `ziv` binary pointed at
//! a REAL (if minimal) HTTP/1.1 server on loopback, so the actual `object_store` retry machinery
//! (the same "Encountered error while reading response body ... Retrying in Xs" path the soak
//! observed against the real IDR store) is what arms the timer this test is about. The mock
//! server answers every level-0 CHUNK request with valid headers (200, an `ETag`, and a
//! `Content-Length` far larger than what it actually sends), a few bytes of body, then holds the
//! connection open for [`CHUNK_STALL`] before closing it without completing the declared length
//! — a connection reset partway through a real remote read, which is exactly what triggers
//! `object_store`'s body-stream retry (`tokio::time::sleep`-driven backoff).
//!
//! There were TWO bugs, both needed for this to reproduce reliably, and both fixed:
//!
//! 1. `server::routes`'s render used to hold its `render_semaphore` permit INLINE in the request
//!    handler's own future, all the way through the render. `tower_http::timeout::TimeoutLayer`'s
//!    30s `DEFAULT_REQUEST_TIMEOUT` drops that future once it fires — which releases the permit
//!    immediately, even though the underlying `spawn_blocking` closure (which cannot be
//!    cancelled) keeps running on its own OS thread, completely undetected. The render now
//!    acquires its permit inline (so an abandoned request still waiting for a permit is still
//!    cancelled the way it always was — no wasted work), then moves the ALREADY-HELD permit into
//!    its own detached `tokio::spawn`ed task before doing the actual (uncancellable) work, so the
//!    permit is held for the render's ACTUAL lifetime regardless of what happens to the request
//!    that triggered it. See `crates/server/src/routes.rs`'s `serve_tile` for the exact shape.
//! 2. `server::DEFAULT_RENDER_DRAIN_TIMEOUT` (the shutdown wait keyed off that same semaphore) was
//!    35s — less than even a single instance of `zarr_core::image::PER_TILE_READ_DEADLINE` (60s),
//!    the render's own worst-case read duration. A render still genuinely within its own budget
//!    could see shutdown give up on it. Now sized to exceed the worst case (2x that deadline, for
//!    an `overlay=` request's sequential image+label reads), and `ZIV_RENDER_DRAIN_TIMEOUT_MS`
//!    is clamped to that same floor so the knob cannot be used to reopen this bug.
//!
//! `CHUNK_STALL` (15s) is sized so that four failed attempts (this crate's `max_retries: 3` —
//! see `zarr_core::store::default_retry_config`) span roughly a minute, comfortably inside
//! `PER_TILE_READ_DEADLINE`'s 60s ceiling on the whole read — long enough to have outlasted the
//! OLD 35s render-drain default (plus the near-instant `ZIV_SHUTDOWN_DRAIN_TIMEOUT_MS` connection
//! drain below) while staying short enough for this test to run in about a minute.
//!
//! `ZIV_RENDER_DRAIN_TIMEOUT_MS` is deliberately NOT set here: this test's pass/fail must depend
//! on the compiled-in `DEFAULT_RENDER_DRAIN_TIMEOUT`, which is fix #2, not on a test-supplied
//! override that would make the outcome independent of it. (Verified against a build with both
//! fixes individually reverted: each one alone still reproduces the panic — see the shutdown-panic
//! report for the three-way matrix.)
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the mock holds each level-0 chunk connection open before breaking the response body.
/// See the module doc comment for why this value matters.
const CHUNK_STALL: Duration = Duration::from_secs(15);

/// Kills the child on drop so a failed assertion never leaks a serving process onto the port.
struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reserves an ephemeral loopback port by binding it and immediately dropping the listener,
/// leaving the port free for `ziv serve` to bind moments later. `ziv serve --addr` cannot itself
/// be given port 0 and report back which port the OS actually chose (it prints the literal
/// `--addr` string, not the resolved socket address), so a fixed port has to be chosen up front —
/// but a HARDCODED one is a real bug, not just untidy: two concurrent runs of this test binary
/// (e.g. manually launched in parallel to stress-test concurrent behaviour, or simply an
/// unlucky port collision with something else on the box) would both bind the SAME address, one
/// would win, and the other would silently become an extra, uncoordinated CLIENT of the winner's
/// server (`wait_until_serving` only checks that ANY server answers on that address) — corrupting
/// both runs in a way that's easy to misattribute to the code under test rather than the harness.
/// There is still a theoretical gap between dropping this listener and `ziv` binding the same
/// port, but it is far narrower than a compile-time constant shared by every run.
fn reserve_ephemeral_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve an ephemeral port");
    listener.local_addr().unwrap().port()
}

fn wait_until_serving(addr: &str, deadline: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A parsed HTTP/1.1 request: just enough to sabotage chunk reads convincingly.
struct MockRequest {
    path: String,
    /// The start offset of a `Range: bytes=X-` header, when present. `object_store`'s
    /// body-stream retry resumes a broken read with exactly this header — a mock that doesn't
    /// honour it (answering 200 instead of 206) makes object_store treat the reply as "range
    /// request not supported", a HARD, non-retryable error that ends the retry loop after a
    /// single attempt instead of letting it run through its full retry budget.
    range_start: Option<u64>,
}

/// Reads (and discards) one HTTP/1.1 request up to its header/body boundary, returning its path
/// and any `Range` start offset. Good enough for `object_store`'s GET-only traffic.
fn read_request(stream: &mut TcpStream) -> Option<MockRequest> {
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
    let mut lines = text.lines();
    let request_line = lines.next()?;
    let raw_path = request_line.split_whitespace().nth(1)?;
    let path = raw_path.split('?').next().unwrap_or(raw_path).to_string();

    let range_start = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("range") {
            return None;
        }
        let spec = value.trim().strip_prefix("bytes=")?;
        spec.split('-').next()?.trim().parse().ok()
    });

    Some(MockRequest { path, range_start })
}

/// Handles one connection: sabotages level-0 chunk requests, serves everything else (metadata,
/// other levels) straight off disk from `root`, and answers anything not found with a plain 404.
/// A zarr v2 chunk key is a run of dot-separated non-negative integers (e.g. `"0.1.0.0.0"`) —
/// unlike every metadata filename zarrs might probe for at this same path (`.zarray`, `.zattrs`,
/// `.zgroup`, or the v3-style `zarr.json` it tries first before falling back to v2), all of which
/// contain a non-digit. Checking for the metadata names by exact match is not enough: zarrs
/// probes `zarr.json` at the SAME path a chunk keyed `0` would use to store, e.g., chunk `0`
/// itself, and any name-based blocklist would need to keep enumerating every metadata filename
/// zarrs might ever probe for. A positive "is this actually a chunk key" check does not have that
/// problem.
fn looks_like_chunk_key(last_segment: &str) -> bool {
    !last_segment.is_empty()
        && last_segment
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The (fake) total size every level-0 chunk claims to be — far more than the handful of bytes
/// any single attempt actually sends, guaranteeing an incomplete body every time.
const FAKE_CHUNK_TOTAL_LEN: u64 = 999_999;

fn handle_connection(mut stream: TcpStream, root: &Path) {
    let Some(req) = read_request(&mut stream) else {
        return;
    };
    let is_level0_chunk = req
        .path
        .strip_prefix("/0/")
        .is_some_and(looks_like_chunk_key);

    if is_level0_chunk {
        // An ETag is REQUIRED for object_store's body-stream retry path to treat a broken body
        // as retryable rather than a hard failure, and a REAL 206/Content-Range answer to the
        // retry's `Range` header is required to keep it retrying instead of giving up early with
        // a hard "range request not supported" error (see `MockRequest::range_start`'s doc
        // comment) — both matter for reliably exhausting the full retry budget rather than
        // stopping after one attempt.
        let header = match req.range_start {
            Some(start) => format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{FAKE_CHUNK_TOTAL_LEN}\r\n\
                 Content-Length: {}\r\nETag: \"mock-chunk\"\r\nConnection: close\r\n\r\n",
                FAKE_CHUNK_TOTAL_LEN - 1,
                FAKE_CHUNK_TOTAL_LEN - start,
            ),
            None => format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {FAKE_CHUNK_TOTAL_LEN}\r\nETag: \"mock-chunk\"\r\n\
                 Connection: close\r\n\r\n",
            ),
        };
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(b"0123456789ABCDEF");
        let _ = stream.flush();
        std::thread::sleep(CHUNK_STALL);
        // Dropping here closes the socket without completing the declared body: a connection
        // reset partway through, indistinguishable to the client from a flaky real remote store.
        return;
    }

    let rel = req.path.trim_start_matches('/');
    match std::fs::read(root.join(rel)) {
        Ok(bytes) => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"mock-meta\"\r\n\
                 Connection: close\r\n\r\n",
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

/// Spawns a background HTTP/1.1 server on loopback that serves `root` (an OME-Zarr v2 directory)
/// verbatim, except that every level-0 chunk request is sabotaged as described in the module doc
/// comment. Returns the bound port.
fn spawn_failing_zarr_http_server(root: PathBuf) -> u16 {
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

/// Continuously drains `reader` into `buf` on a background thread, so a chatty child process
/// (tracing JSON logs plus a panic message and backtrace) can never block on a full pipe while
/// nothing is reading it.
fn spawn_pipe_reader<R: Read + Send + 'static>(mut reader: R, buf: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut text) = buf.lock() {
                        text.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                }
            }
        }
    });
}

/// Drives the actual reproduction: starts the real `ziv` binary against a stalling store, puts a
/// real request for `/iiif/default/full/max/0/default.jpg` in flight, sends `phantom_clients`
/// EXTRA concurrent requests for the exact same URL that each disconnect abruptly shortly after
/// (simulating abandoned/interrupted callers sharing the one coalesced render — see
/// `concurrent_clients_disconnecting_mid_render_do_not_cause_a_panic`'s doc comment for why this
/// matters), sends real `SIGTERM`, and asserts the process exits 0 with no `panicked at` in its
/// stderr.
fn run_stalling_read_shutdown_scenario(phantom_clients: usize) {
    let fixture = std::fs::canonicalize("../../tests/fixtures/sample_v04.ome.zarr")
        .expect("fixture directory must exist");
    let mock_port = spawn_failing_zarr_http_server(fixture);
    let store_url = format!("http://127.0.0.1:{mock_port}/");

    let port = reserve_ephemeral_port();
    let addr_string = format!("127.0.0.1:{port}");
    let addr = addr_string.as_str();
    let mut child = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_ziv"))
            .args([
                "serve",
                &store_url,
                "--addr",
                addr,
                "--allow-internal-hosts",
            ])
            // Shrinks only the CONNECTION-drain wait, so the test doesn't also pay
            // `DEFAULT_SHUTDOWN_DRAIN_TIMEOUT`'s full 20s on top of the render-drain window this
            // test is actually about. `ZIV_RENDER_DRAIN_TIMEOUT_MS` is deliberately left unset —
            // see the module doc comment.
            .env("ZIV_SHUTDOWN_DRAIN_TIMEOUT_MS", "50")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn the ziv binary"),
    );

    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    spawn_pipe_reader(child.0.stdout.take().unwrap(), stdout_buf.clone());
    spawn_pipe_reader(child.0.stderr.take().unwrap(), stderr_buf.clone());

    assert!(
        wait_until_serving(addr, Duration::from_secs(30)),
        "ziv did not start serving on {addr}\nstdout:\n{}\nstderr:\n{}",
        stdout_buf.lock().unwrap(),
        stderr_buf.lock().unwrap()
    );

    const REQUEST: &[u8] =
        b"GET /iiif/default/full/max/0/default.jpg HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";

    // `phantom_clients` extra callers for the EXACT same URL: `moka::try_get_with` coalesces them
    // onto the one render already covered by `sock` below, so this exercises several concurrent
    // futures sharing that render, each capable of being dropped independently.
    let mut phantoms: Vec<TcpStream> = (0..phantom_clients)
        .map(|_| {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(REQUEST).unwrap();
            s.flush().unwrap();
            s
        })
        .collect();
    // Give them a moment to be accepted and start sharing the in-flight render, then abandon them
    // mid-flight by dropping their sockets — an abrupt client disconnect, indistinguishable from
    // a real one going away, while the shared render is still stalling.
    std::thread::sleep(Duration::from_millis(30));
    phantoms.clear();

    // Put a real request in flight against the stalling store — a full-image render, which
    // touches every level-0 chunk (this fixture's 2 channels x 2x2 spatial chunks) concurrently.
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.write_all(REQUEST).unwrap();
    sock.flush().unwrap();

    // SIGTERM shortly after the request lands, while the remote reads are still in their first
    // attempt — matching the real soak, where the signal was sent independent of request state.
    std::thread::sleep(Duration::from_millis(50));
    let pid = child.0.id();
    let killed = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("failed to run kill");
    assert!(killed.success(), "kill -TERM failed for pid {pid}");

    // Generous: worst case is a handful of ~15s stalled attempts plus the drain timeouts.
    let started = Instant::now();
    let status = loop {
        match child.0.try_wait().unwrap() {
            Some(status) => break status,
            None if started.elapsed() > Duration::from_secs(150) => {
                panic!(
                    "ziv did not exit within 150s of SIGTERM during a stalling remote read \
                     (stdout so far: {})",
                    stdout_buf.lock().unwrap()
                );
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };
    // Let the reader threads catch up on whatever the process wrote right before exiting.
    std::thread::sleep(Duration::from_millis(200));
    assert!(status.success(), "ziv exited non-zero: {status:?}");

    let stderr_text = stderr_buf.lock().unwrap().clone();
    assert!(
        !stderr_text.contains("panicked at"),
        "ziv panicked during shutdown while a remote read was stalling — stderr:\n{stderr_text}"
    );
}

#[test]
fn sigterm_during_a_stalling_remote_read_does_not_panic() {
    run_stalling_read_shutdown_scenario(0);
}

/// The concurrency shape that first exposed the permit-acquired-inside-the-spawned-task bug in
/// review: several concurrent callers sharing ONE coalesced render (`moka::try_get_with`), some
/// of which disconnect (are dropped) WHILE that render is still running. If the render's permit
/// were tied to any ONE caller's future rather than to a task detached before the permit is ever
/// taken, this is exactly the shape that made `drain_renders` observe "all permits free" while
/// the underlying `spawn_blocking` thread was still alive and mid-retry against the stalling
/// store — the same panic as the single-client test, just via a different, load-shaped path to
/// the same root cause. See `crates/server/src/routes.rs`'s `serve_tile` doc comment and
/// `a_request_cancelled_while_queued_for_a_permit_never_renders` for the unit-level version of
/// this same property.
#[test]
fn concurrent_clients_disconnecting_mid_render_do_not_cause_a_panic() {
    run_stalling_read_shutdown_scenario(3);
}
