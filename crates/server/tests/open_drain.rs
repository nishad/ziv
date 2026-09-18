//! Reproduction of the shutdown-panic bug shape (found by a one-hour remote soak; see `docs/operations.md`) in the
//! IMAGE-OPEN path rather than the render path the soak originally found it in.
//!
//! `registry::ImageRegistry::get`'s lazy open used to acquire its `open_semaphore` permit INLINE
//! in the future `moka::future::Cache::try_get_with` polls directly inside whichever caller's task
//! first misses the cache (`moka`'s `value_initializer.rs` has no internal spawn insulating that
//! future from the caller going away — see `registry.rs`'s `ImageRegistry::get` doc comment). A
//! caller cancelled mid-open — `tower_http::timeout::TimeoutLayer`'s 30s firing, or simply
//! disconnecting — drops that future, which released the permit immediately even though the
//! underlying `spawn_blocking` closure (which cannot itself be cancelled) kept running on its own
//! OS thread, driving `object_store` retries that arm `tokio::time::sleep` timers long after the
//! permit that was supposed to represent them was gone. That is the exact shape the render path
//! had before its own fix.
//!
//! This drives the REAL `ImageRegistry::get` (not a synthetic stand-in — there is no trait DI seam
//! for "the open work" the way `TileEngine` is one for renders) against a minimal, always-503
//! HTTP/1.1 mock server on loopback, so `object_store`'s real retry/backoff machinery is what arms
//! the timer this test is about — the same mechanism `crates/cli/tests/shutdown_panic.rs` proves
//! end-to-end for renders, but sped up: a 503 fails every attempt IMMEDIATELY (no chunk-stall
//! needed), so the full retry sequence (`default_retry_config`'s `max_retries: 3`, i.e. 4 total
//! attempts, backing off 100/200/400ms as `object_store::client::backoff::BackoffConfig`'s
//! defaults dictate) finishes in well under a second — fast enough to run as an ordinary test, not
//! a 65-second process test.
//!
//! The mock server signals the moment it has answered the FINAL (4th) attempt via a one-shot
//! channel. That gives a precise, non-timing-based way to tell "did the drain report the permit
//! free BEFORE or AFTER the real work actually finished" — no wall-clock thresholds needed:
//! against the broken shape the drain returns before that signal ever fires (the permit was freed
//! the instant the caller was aborted); against the fixed shape the drain cannot return before it.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use server::name::ImageName;
use server::registry::{ExplicitSource, ImageRegistry};

/// `default_retry_config` fixes `max_retries: 3` (asserted by
/// `zarr_core::store::default_retry_config_bounds_retries_to_three`), so exactly 4 total attempts
/// (1 initial + 3 retries) for a store that fails every single one.
const EXPECTED_ATTEMPTS: usize = 4;

/// Spawns a minimal HTTP/1.1 server on loopback that answers every request with `503 Service
/// Unavailable` (server errors are retryable per `object_store`'s own classification — see
/// `object_store::client::retry::RetryableRequest::send`) and never actually succeeds. Serves
/// exactly [`EXPECTED_ATTEMPTS`] connections then its thread exits. `notify` fires after every
/// response (so the test can observe "at least one attempt happened" without a wall-clock guess);
/// the returned `oneshot::Receiver` fires once, with the `Instant` the FINAL attempt was answered
/// at, for the precise "did the drain outlive the real work" check.
fn spawn_always_503(
    notify: Arc<tokio::sync::Notify>,
) -> (String, tokio::sync::oneshot::Receiver<Instant>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().unwrap();
    let (final_tx, final_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut final_tx = Some(final_tx);
        for attempt in 1..=EXPECTED_ATTEMPTS {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            // Drain the request up to the header/body boundary; a metadata GET has no body, so
            // this is the whole request.
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let read = stream.read(&mut buf).unwrap_or(0);
                if read == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..read]);
                if seen.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let response =
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let answered_at = Instant::now();
            notify.notify_one();
            if attempt == EXPECTED_ATTEMPTS {
                if let Some(tx) = final_tx.take() {
                    let _ = tx.send(answered_at);
                }
            }
        }
    });
    (
        format!("http://127.0.0.1:{}/flaky.ome.zarr", addr.port()),
        final_rx,
    )
}

fn registry_over(url: String) -> Arc<ImageRegistry> {
    let source = ExplicitSource::from_pairs(vec![(ImageName::parse("flaky").unwrap(), url)])
        .expect("a single-entry catalogue cannot collide");
    Arc::new(
        ImageRegistry::builder()
            .source(Box::new(source))
            // One open permit: irrelevant to which permit is held, but keeps `open_permits()`
            // small so `drain_opens` acquiring "every permit" is a single, easily-reasoned-about
            // acquisition.
            .max_concurrent_opens(1)
            // 127.0.0.1 is loopback; the SSRF guard blocks it unless explicitly allowed, exactly
            // as it should in production.
            .allow_internal_hosts(true)
            .build(),
    )
}

/// CONTROL (the drain invariant): a caller aborted mid-open must not let `drain_opens` report
/// "no open still running" while the real `spawn_blocking` open — driving `object_store`'s retry
/// loop on its own OS thread — is provably still executing.
///
/// Against today's (pre-fix) `registry.rs`, the permit is released the instant the caller task is
/// aborted, so `drain_opens` returns before the mock server has answered anywhere near its final
/// attempt: `final_rx.try_recv()` is still empty at that point. Against the fix, the permit is
/// held by a detached task tied to the open's real completion, so `drain_opens` cannot return
/// before the final attempt has already been answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_opens_does_not_report_success_while_an_open_still_runs() {
    let notify = Arc::new(tokio::sync::Notify::new());
    let (url, mut final_rx) = spawn_always_503(notify.clone());
    let registry = registry_over(url);
    let sem = registry.open_semaphore();
    let permits = registry.open_permits();
    let name = ImageName::parse("flaky").unwrap();

    let task_registry = registry.clone();
    let handle = tokio::spawn(async move {
        let _ = task_registry.get(&name).await;
    });

    // Wait for the first attempt to land — proves the open has genuinely started (past name
    // resolution, past acquiring the permit, into the actual remote call) before we cancel it.
    notify.notified().await;

    // The caller goes away mid-open. The real open (spawn_blocking, driving object_store's
    // retries) keeps running regardless — it cannot be cancelled.
    handle.abort();
    let _ = handle.await;

    server::drain_opens(&sem, permits, Duration::from_secs(5)).await;

    assert!(
        final_rx.try_recv().is_ok(),
        "INVARIANT VIOLATED: drain_opens reported \"no open still running\" before the mock \
         server had even answered its final ({EXPECTED_ATTEMPTS}th) attempt — the permit was \
         released while the open was provably still retrying"
    );
}

/// CONTROL (the literal panic): reproduces the soak's crash end to end for the open path. A
/// caller goes away mid-open; `drain_opens` is given the whole (generous, 5s — comfortably more
/// than the well-under-a-second worst case four immediate 503s produce) budget; whatever it
/// decides, the runtime is dropped — the exact `run_server` -> `main` -> `#[tokio::main]` drop
/// sequence. If the open is still in flight at that point and polls a `tokio` timer (exactly what
/// `object_store`'s retry backoff does), tokio panics with "A Tokio 1.x context was found, but it
/// is being shutdown."
#[test]
fn runtime_drop_after_open_drain_does_not_panic() {
    let panicked = Arc::new(AtomicBool::new(false));
    let flag = panicked.clone();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!("{info}");
        if msg.contains("A Tokio 1.x context") || msg.contains("being shutdown") {
            flag.store(true, Ordering::SeqCst);
        }
        eprintln!("PANIC HOOK SAW: {msg}");
    }));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    let last_attempt_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    rt.block_on(async {
        let notify = Arc::new(tokio::sync::Notify::new());
        let (url, final_rx) = spawn_always_503(notify.clone());
        let registry = registry_over(url);
        let sem = registry.open_semaphore();
        let permits = registry.open_permits();
        let name = ImageName::parse("flaky").unwrap();

        let task_registry = registry.clone();
        let handle = tokio::spawn(async move {
            let _ = task_registry.get(&name).await;
        });
        notify.notified().await;
        handle.abort();
        let _ = handle.await;

        let t0 = Instant::now();
        server::drain_opens(&sem, permits, Duration::from_secs(5)).await;
        eprintln!("drain_opens returned after {:?}", t0.elapsed());

        if let Ok(at) = final_rx.await {
            *last_attempt_at.lock().unwrap() = Some(at);
        }
    });

    // The exact production sequence: run_server returned, main's async body returned,
    // #[tokio::main] drops the Runtime.
    drop(rt);
    std::panic::set_hook(prev);
    eprintln!(
        "final attempt recorded: {:?}",
        last_attempt_at.lock().unwrap()
    );
    assert!(
        !panicked.load(Ordering::SeqCst),
        "TARGET PANIC REPRODUCED: an open was still polling a tokio timer inside spawn_blocking \
         when the runtime was dropped"
    );
}
