//! Connection-phase bounds that the request-level governance layers structurally cannot provide.
//!
//! `GlobalConcurrencyLimitLayer` and `TimeoutLayer` both act on an in-flight REQUEST. A connection
//! that is still sending its headers never becomes one, so before ziv owned its accept loop a
//! client could open a socket, send half a request line, and hold the connection indefinitely —
//! measured at over 45 seconds against the release binary. The cause was subtle: hyper nominally
//! defaults `h1_header_read_timeout` to 30s, but drops that default unless a `Timer` is installed
//! on the connection builder, and `axum::serve` installs none. Its own warning about this is
//! compiled out because hyper's `tracing` feature is off, so nothing surfaced it.
//!
//! These tests exercise the real accept loop (`serve_with_connection_limits`) over real TCP
//! sockets rather than `oneshot`-ing the router, because the property under test lives in the
//! connection lifecycle, below the router entirely.
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn app() -> axum::Router {
    let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
    server::app(server::AppState::new(
        Arc::new(tiling::ZarrTileEngine::new(img)),
        "http://test".into(),
    ))
}

/// Binds an ephemeral port and runs the real accept loop on it, returning the address and a
/// handle plus the trigger that stops it.
async fn spawn_server(
    header_read_timeout: Duration,
    max_connections: usize,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<std::io::Result<()>>,
    tokio::sync::oneshot::Sender<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        server::serve_with_connection_limits(
            listener,
            app(),
            header_read_timeout,
            max_connections,
            async {
                let _ = rx.await;
            },
            server::DEFAULT_SHUTDOWN_DRAIN_TIMEOUT,
        )
        .await
    });
    (addr, handle, tx)
}

/// The slowloris property: a client that sends a partial request line and then stops must be
/// disconnected by the server, not held forever. Before the accept loop this hung indefinitely.
#[tokio::test]
async fn half_sent_request_headers_are_disconnected_by_the_header_read_timeout() {
    let (addr, handle, _stop) = spawn_server(Duration::from_secs(2), 64).await;

    let mut sock = TcpStream::connect(addr).await.unwrap();
    // A request line with no terminating blank line: hyper stays in the header-read phase.
    sock.write_all(b"GET /iiif/default/info.json HTTP/1.1\r\n")
        .await
        .unwrap();
    sock.flush().await.unwrap();

    let started = Instant::now();
    let mut buf = Vec::new();
    // Either a 408 followed by close, or a bare close — both are the server ending it. What must
    // NOT happen is this read hanging until the outer timeout.
    let read = tokio::time::timeout(Duration::from_secs(15), sock.read_to_end(&mut buf)).await;
    let elapsed = started.elapsed();

    assert!(
        read.is_ok(),
        "server never closed a half-sent-header connection: still open after 15s"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "connection was closed, but took {elapsed:?} — the 2s header-read timeout is not in effect"
    );

    handle.abort();
}

/// Control: the timeout must bound only the HEADER phase. A complete request on a keep-alive
/// connection still gets served normally, and the connection is not torn down mid-response.
#[tokio::test]
async fn a_complete_request_is_served_normally() {
    let (addr, handle, _stop) = spawn_server(Duration::from_secs(2), 64).await;

    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        b"GET /iiif/default/info.json HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), sock.read_to_end(&mut buf))
        .await
        .expect("a complete request must be answered")
        .unwrap();
    let response = String::from_utf8_lossy(&buf);
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "expected 200, got: {}",
        response.lines().next().unwrap_or("<empty>")
    );
    assert!(response.contains("\"width\""), "expected info.json body");

    handle.abort();
}

/// Graceful shutdown must complete even with an idle keep-alive connection parked on the server.
/// A drain that waits unconditionally for every connection to close hangs here forever, since an
/// idle keep-alive peer has no reason to hang up.
#[tokio::test]
async fn shutdown_completes_with_an_idle_keepalive_connection_open() {
    let (addr, handle, stop) = spawn_server(Duration::from_secs(30), 64).await;

    // Complete one request but keep the connection open (no `Connection: close`).
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"GET /iiif/default/info.json HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(15), sock.read(&mut buf))
        .await
        .expect("first response must arrive")
        .unwrap();
    assert!(n > 0, "expected a response on the keep-alive connection");

    let _ = stop.send(());
    let result = tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect(
            "serve loop must return after the shutdown trigger, not hang on an idle connection",
        );
    result.unwrap().unwrap();
}

/// The connection cap is a real bound: with room for one connection, a second connection cannot
/// be served while the first is held open. It becomes servable as soon as the first is dropped,
/// which is what proves the loop caps live connections rather than refusing outright.
#[tokio::test]
async fn connection_cap_bounds_simultaneous_connections() {
    let (addr, handle, _stop) = spawn_server(Duration::from_secs(30), 1).await;

    // Hold connection #1 open, mid-header so it stays alive and keeps its permit.
    let mut held = TcpStream::connect(addr).await.unwrap();
    held.write_all(b"GET /iiif/default/info.json HTTP/1.1\r\n")
        .await
        .unwrap();
    held.flush().await.unwrap();
    // Give the accept loop time to accept it and take the only permit.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Connection #2: the TCP handshake still succeeds (the kernel backlog accepts it), but the
    // server must not serve it while the cap is taken.
    let mut second = TcpStream::connect(addr).await.unwrap();
    second
        .write_all(
            b"GET /iiif/default/info.json HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut buf = [0u8; 512];
    let starved = tokio::time::timeout(Duration::from_secs(2), second.read(&mut buf)).await;
    assert!(
        starved.is_err(),
        "second connection was served while the 1-connection cap was already taken"
    );

    // Free the permit; the queued connection is then picked up and answered.
    drop(held);
    let n = tokio::time::timeout(Duration::from_secs(15), second.read(&mut buf))
        .await
        .expect("queued connection must be served once a slot frees")
        .unwrap();
    assert!(n > 0, "expected a response after a slot freed");

    handle.abort();
}

/// Shutdown must not return while a render is still running.
///
/// This is the property behind a panic seen in the wild. A render runs on the blocking pool via
/// `spawn_blocking`, which cannot be cancelled, and a remote read inside it drives `object_store`,
/// which arms `tokio` timers for its retry backoff. Returning from `main` in that state drops the
/// runtime underneath the task, and the next timer it arms panics with "A Tokio 1.x context was
/// found, but it is being shutdown".
///
/// Draining CONNECTIONS does not imply this and is not enough: the client can be long gone —
/// timed out, or disconnected — while the work it started is still reading from a remote store.
/// So this holds the permit a render would hold, triggers shutdown, and asserts the serve path
/// waits rather than returning underneath it.
#[tokio::test]
async fn shutdown_waits_for_an_in_flight_render() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    // One permit, so holding it is unambiguous: exactly one render is in flight.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let in_flight_render = semaphore.clone().acquire_owned().await.unwrap();

    let sem = semaphore.clone();
    let handle = tokio::spawn(async move {
        server::serve_with_connection_limits(
            listener,
            app(),
            Duration::from_secs(30),
            64,
            async {
                let _ = rx.await;
            },
            server::DEFAULT_SHUTDOWN_DRAIN_TIMEOUT,
        )
        .await
        .unwrap();
        // Exactly what `run_server` does once the connections have drained.
        server::drain_renders(&sem, 1, Duration::from_secs(10)).await;
    });

    let _ = tx.send(());

    // Connections drain immediately — there are none — so without the render drain the task would
    // already be finished here.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !handle.is_finished(),
        "shutdown returned while a render was still holding its permit"
    );

    drop(in_flight_render); // the render completes
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("shutdown must finish once no render is in flight")
        .unwrap();
}

/// ...but it must not wait forever. A render wedged on an unreachable store cannot be allowed to
/// stop the process exiting, so the drain gives up after its deadline.
#[tokio::test]
async fn the_render_drain_gives_up_after_its_deadline() {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let _never_released = semaphore.clone().acquire_owned().await.unwrap();

    let started = std::time::Instant::now();
    server::drain_renders(&semaphore, 1, Duration::from_millis(300)).await;
    let waited = started.elapsed();

    assert!(
        waited >= Duration::from_millis(250),
        "must actually wait for the deadline, waited {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(3),
        "must give up at the deadline rather than hang, waited {waited:?}"
    );
}
