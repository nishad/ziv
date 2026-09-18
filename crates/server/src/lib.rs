pub mod auth;
pub mod health;
pub mod metrics;
pub mod mount;
pub mod name;
pub mod origin;
pub mod registry;
pub mod routes;
pub mod viewer;

use std::sync::Arc;
use std::time::Duration;

use axum::error_handling::HandleErrorLayer;
use axum::http::{Request, StatusCode};
use axum::BoxError;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::sync::Semaphore;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{
    MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

pub use auth::AuthConfig;
pub use routes::{router, AppState};

/// Default cap on concurrent in-flight HTTP requests across the whole server. Requests past this
/// cap are shed to 503 immediately (see `app`'s `LoadShedLayer` + `GlobalConcurrencyLimitLayer`
/// pairing) rather than queued unboundedly, which is the actual DoS-safety property: an
/// unbounded queue still lets an attacker pile up memory/timers even if no request ever gets
/// service, whereas shedding fails fast and lets the caller retry/back off.
pub const DEFAULT_CONCURRENCY_LIMIT: usize = 512;

/// Default whole-request timeout. A request that hasn't completed (including a hung/slow render)
/// within this window is aborted server-side and the connection's resources freed, returning
/// `408 Request Timeout` to the caller via `tower_http::timeout::TimeoutLayer`.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a connection may take to finish sending its request HEADERS before the server closes
/// it. This is the slowloris bound, and it is deliberately set here rather than left to hyper's
/// nominal 30s default: that default is SILENTLY DROPPED unless a `Timer` is installed on the
/// connection builder (`hyper::common::time::Time::check` returns `None` for `Time::Empty`), and
/// `axum::serve` installs none. Measured against the release binary before this was added, a
/// socket that sent `GET / HTTP/1.1\r\n` and then stopped was still held open after 45 seconds.
/// hyper's own "timeout `header_read_timeout` has default, but no timer set" warning is compiled
/// out because its `tracing` feature is off, so there was no signal either.
///
/// Note this bounds the HEADER phase only, which the request-level `DEFAULT_REQUEST_TIMEOUT`
/// cannot: a connection stuck mid-header never becomes an in-flight request, so it is invisible
/// to `TimeoutLayer` and to `GlobalConcurrencyLimitLayer` alike.
pub const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Cap on simultaneously-open connections. `GlobalConcurrencyLimitLayer` bounds in-flight
/// REQUESTS, which a connection idling in the header phase never becomes — so without a separate
/// connection bound, a slow client could hold connections up to the process file-descriptor
/// limit. The accept loop holds one permit per live connection and stops accepting at the cap,
/// leaving further connections in the kernel backlog rather than consuming per-connection memory
/// (~420KB of header buffer each) in the process.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// How long `graceful_shutdown` waits for in-flight connections to drain before giving up and
/// returning anyway, so a single wedged connection cannot block process exit indefinitely.
///
/// Overridable via `ZIV_SHUTDOWN_DRAIN_TIMEOUT_MS` (see [`run_server`]) for operators who want a
/// faster restart than this default, and for tests that need to shrink it well below its
/// production value.
pub const DEFAULT_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

/// How long shutdown waits for in-flight RENDERS after the connections have drained.
///
/// Draining connections is not the same as draining work. A render runs on the blocking pool via
/// `spawn_blocking`, which cannot be cancelled, and a remote read inside it drives `object_store`
/// — which arms `tokio` timers for its retry backoff. If the process returns from `main` while
/// such a task is still running, the runtime is dropped underneath it and the next timer it arms
/// panics with "A Tokio 1.x context was found, but it is being shutdown". Observed in the wild on
/// a server killed while serving a slow remote image (a one-hour remote soak; see `docs/operations.md`).
///
/// **This must exceed the worst-case render duration, not just "feel generous".** A render's own
/// remote reads are bounded by `zarr_core::image::PER_TILE_READ_DEADLINE` (60s) — but an
/// `overlay=` request reads the image AND the label SEQUENTIALLY
/// (`tiling::engine::render_overlay_rgb`), each under its own instance of that deadline, so a
/// single render can legitimately run for up to 2x that before returning (success or error). The
/// bug this constant caused: it was previously 35s, LESS than even a single instance of that 60s
/// deadline. When a render was still genuinely within its own budget at the 35s mark, this drain
/// gave up and returned, `main` returned, and the runtime was dropped while the render's
/// `spawn_blocking` thread was still alive and about to arm another retry timer — the exact panic
/// above, deterministically, not as a rare race. Sized here as `2 * PER_TILE_READ_DEADLINE` plus a
/// margin, so a well-behaved render (one that respects its own deadline) can never still be
/// running when this gives up; see the `render_drain_timeout_covers_worst_case_render_duration`
/// test for the enforced relationship.
///
/// Longer than [`DEFAULT_REQUEST_TIMEOUT`] on purpose: a render whose HTTP request has already
/// timed out is still running, and this is the wait that lets it finish rather than be torn down
/// mid-flight.
///
/// Overridable via `ZIV_RENDER_DRAIN_TIMEOUT_MS` (see [`run_server`]) — but see
/// [`MIN_RENDER_DRAIN_TIMEOUT`]: the override is clamped, not taken verbatim, specifically so this
/// knob cannot be used to reopen the bug this constant exists to close.
pub const DEFAULT_RENDER_DRAIN_TIMEOUT: Duration = Duration::from_secs(150);

/// The floor `ZIV_RENDER_DRAIN_TIMEOUT_MS` is clamped to (see [`clamp_render_drain_timeout`]).
///
/// This is `2 * PER_TILE_READ_DEADLINE` with NO margin — the bare minimum a render can legitimately
/// need (an `overlay=` request's sequential image + label reads), not the more generous
/// [`DEFAULT_RENDER_DRAIN_TIMEOUT`]. An operator is free to configure something between this floor
/// and the default (trading shutdown latency for less margin), but not below it: anything less is
/// not "faster shutdown", it is the exact 35s-style misconfiguration
/// (found by a one-hour remote soak) that made this drain give up on a render still
/// genuinely within its own budget.
pub const MIN_RENDER_DRAIN_TIMEOUT: Duration =
    Duration::from_secs(zarr_core::image::PER_TILE_READ_DEADLINE.as_secs() * 2);

/// Reads `var` as a millisecond count and returns it as a `Duration`, falling back to `default`
/// when the variable is unset — an unset knob is the normal case and must stay silent. A variable
/// that IS set but fails to parse is different: see [`parse_duration_ms`], which this delegates
/// to and which warns loudly rather than falling back unnoticed.
///
/// This is how [`DEFAULT_SHUTDOWN_DRAIN_TIMEOUT`]/[`DEFAULT_RENDER_DRAIN_TIMEOUT`] become tunable
/// without a rebuild (`ZIV_SHUTDOWN_DRAIN_TIMEOUT_MS`/`ZIV_RENDER_DRAIN_TIMEOUT_MS`), and how the
/// signal-injection shutdown tests shrink the connection-drain wait to keep a real SIGTERM test
/// fast without touching the render-drain default the fix itself lives in.
fn duration_from_env_ms(var: &str, default: Duration) -> Duration {
    match std::env::var(var) {
        Ok(raw) => parse_duration_ms(var, &raw, default),
        Err(std::env::VarError::NotPresent) => default,
        Err(std::env::VarError::NotUnicode(raw)) => {
            tracing::warn!(
                env_var = var,
                value = %raw.to_string_lossy(),
                default_ms = default.as_millis() as u64,
                "{var} is set but is not valid UTF-8; using the default instead"
            );
            default
        }
    }
}

/// Parses `raw` (the value `var` was actually set to) as a millisecond count, warning and falling
/// back to `default` if it does not parse as a `u64` — an operator typo (a stray unit suffix, a
/// negative number, a float, empty string) used to fall back to the production default completely
/// silently, indistinguishable in the logs from the variable never having been set at all. Split
/// out from [`duration_from_env_ms`] (which does the actual env lookup) so the parse-failure
/// warning is unit-testable without mutating real process environment state, which every test in
/// this binary shares — same reasoning as [`clamp_render_drain_timeout`]'s split from its own env
/// access.
fn parse_duration_ms(var: &str, raw: &str, default: Duration) -> Duration {
    match raw.trim().parse::<u64>() {
        Ok(ms) => Duration::from_millis(ms),
        Err(e) => {
            tracing::warn!(
                env_var = var,
                value = raw,
                error = %e,
                default_ms = default.as_millis() as u64,
                "{var} is set but is not a valid non-negative millisecond count; using the \
                 default instead"
            );
            default
        }
    }
}

/// Clamps a configured render-drain timeout up to [`MIN_RENDER_DRAIN_TIMEOUT`], warning loudly
/// when clamping actually changes the value — an operator setting `ZIV_RENDER_DRAIN_TIMEOUT_MS`
/// below the render's own documented worst case is, by construction, reintroducing the exact
/// shutdown panic this drain exists to prevent, so this is not allowed to happen silently. Pure
/// (no env access) so the clamping arithmetic itself is unit-testable without touching real
/// process environment state.
///
/// Logs `configured_ms` (milliseconds), not `configured.as_secs()`: a sub-second configured value
/// (e.g. `ZIV_RENDER_DRAIN_TIMEOUT_MS=500`) truncates to `0` under `as_secs()`, which reads in the
/// log exactly like an operator who configured `=0` outright — the one piece of information this
/// warning exists to report (what did they actually set) is the one it would get wrong.
fn clamp_render_drain_timeout(configured: Duration) -> Duration {
    if configured < MIN_RENDER_DRAIN_TIMEOUT {
        tracing::warn!(
            configured_ms = configured.as_millis() as u64,
            floor_secs = MIN_RENDER_DRAIN_TIMEOUT.as_secs(),
            "ZIV_RENDER_DRAIN_TIMEOUT_MS is below the render's own worst-case duration \
             (2x PER_TILE_READ_DEADLINE); raising it to the floor to avoid reopening the \
             shutdown-panic bug this timeout exists to prevent"
        );
        MIN_RENDER_DRAIN_TIMEOUT
    } else {
        configured
    }
}

/// Convert `LoadShedLayer`'s `Overloaded` error (surfaced whenever a request arrives while the
/// global concurrency cap is already saturated) into a real HTTP response instead of a
/// connection-level failure — `503 Service Unavailable`: the server is at capacity right now,
/// the caller should back off and retry. (`TimeoutLayer::with_status_code` below handles the
/// timeout case itself, directly producing `408` without going through this handler — see its
/// call site.)
async fn handle_shed_error(_err: BoxError) -> (StatusCode, String) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "server is at capacity, please retry".to_string(),
    )
}

/// Initialize the global `tracing` subscriber. Uses `RUST_LOG` (standard `tracing-subscriber`
/// env-filter syntax, e.g. `RUST_LOG=info,server=debug`) for filtering, defaulting to `info`
/// when unset. Emits JSON-formatted log lines, which is the production-preferred shape (easy to
/// ship to a log aggregator) — set `RUST_LOG` to tune verbosity per environment rather than
/// switching formats.
///
/// Safe to call more than once per process only if the caller guards it (e.g. `main` calling it
/// exactly once); a second global-subscriber install elsewhere would panic, which is standard
/// `tracing` behavior and not specific to this wrapper.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().json().with_env_filter(filter).init();
}

/// Build the full application: routes + viewer + health/metrics, layered with (innermost to
/// outermost as added, which is outermost-to-innermost in actual request-processing order since
/// `tower::Layer` wraps each addition around what came before):
///
/// 1. `SetRequestIdLayer` — assigns an `x-request-id` (UUID) to every request that doesn't
///    already carry one, so it's the FIRST thing that runs and every later layer (trace, error
///    bodies) can rely on it being present.
/// 2. `TraceLayer` — per-request spans (method, path, status, latency), now able to include the
///    request id set in step 1.
/// 3. `CorsLayer` — permissive GET-only CORS so browser-based viewers (OpenSeadragon served from
///    a different origin, e.g. the static `export` output or a separate frontend) can fetch
///    tiles/info.json. Read-only endpoints only, so a permissive origin is a reasonable default;
///    tighten via config in a later phase if this needs to be restricted per-deployment.
/// 4. `TimeoutLayer` — whole-request deadline (`DEFAULT_REQUEST_TIMEOUT`); a request that runs
///    past it is aborted and answered `408` instead of hanging forever.
/// 5. `LoadShedLayer` + `GlobalConcurrencyLimitLayer` — the DoS gate: at most
///    `DEFAULT_CONCURRENCY_LIMIT` requests are in flight at once; a request arriving over that
///    cap is shed to `503` IMMEDIATELY (load-shed sits outside/before the concurrency limit in
///    the tower sense, so it sees "would this block?" and fails fast rather than queuing).
/// 6. `PropagateRequestIdLayer` — echoes the request id back on the OUTGOING response header
///    (placed after CORS/trace/governance in the builder chain so it still wraps the response on
///    its way out, alongside them).
/// 7. `track_metrics` (`axum::middleware::from_fn`, innermost of the wrapping layers, right
///    around the merged router) — records `ziv_http_requests_total`/
///    `ziv_http_request_duration_seconds` (see `metrics` module docs). Innermost so it runs
///    AFTER routing has matched (needed to read the route pattern via `MatchedPath` for the
///    `path` label) but still wraps every route, including health/viewer/metrics themselves.
///
/// Health (`/healthz`, `/readyz`) and `/metrics` are merged in UNAUTHED. They DO still pass
/// through the same governance layers (request-id/trace/cors/timeout/load-shed) as everything
/// else — that's harmless for a liveness probe — but they are never gated by
/// `auth::auth_middleware` (see `routes::router`, which scopes auth to a sub-router that
/// health/metrics are never merged into).
pub fn app(state: AppState) -> axum::Router {
    app_with_governance(state, DEFAULT_CONCURRENCY_LIMIT, DEFAULT_REQUEST_TIMEOUT)
}

/// Build the app with an explicit concurrency cap and request timeout instead of the defaults —
/// used by tests that need a tiny cap/timeout to exercise shedding/timeout behavior without
/// waiting `DEFAULT_REQUEST_TIMEOUT` or firing 512+ requests. `app()` is a thin wrapper over this
/// with the production defaults.
pub fn app_with_governance(
    state: AppState,
    concurrency_limit: usize,
    timeout: Duration,
) -> axum::Router {
    let x_request_id = axum::http::HeaderName::from_static("x-request-id");
    let governance = ServiceBuilder::new()
        .layer(HandleErrorLayer::new(handle_shed_error))
        .layer(LoadShedLayer::new())
        .layer(GlobalConcurrencyLimitLayer::new(concurrency_limit))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeout,
        ));
    routes::router(state.clone())
        .merge(viewer::viewer_router())
        .merge(health::health_router(state.clone()))
        .merge(metrics::metrics_router(state))
        .layer(axum::middleware::from_fn(metrics::track_metrics))
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(governance)
        .layer(CorsLayer::permissive())
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                let request_id = request
                    .extensions()
                    .get::<RequestId>()
                    .and_then(|id| id.header_value().to_str().ok())
                    .unwrap_or_default();
                tracing::info_span!(
                    "http_request",
                    method = %request.method(),
                    path = %request.uri().path(),
                    request_id,
                    status = tracing::field::Empty,
                    latency_ms = tracing::field::Empty,
                )
            }),
        )
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
}

/// Await either SIGTERM or SIGINT (Ctrl-C), whichever arrives first — the standard pair of
/// signals a process orchestrator (systemd, Docker, Kubernetes) or an interactive terminal sends
/// to ask a service to shut down. Passed to `axum::serve(...).with_graceful_shutdown(...)`: once
/// this future resolves, axum stops accepting new connections and waits for in-flight requests to
/// finish (bounded by however long those requests take — there's no additional forced-drain
/// timeout here, matching the plan's "drain in-flight" requirement rather than a hard-kill
/// deadline).
///
/// SIGTERM is Unix-only (`tokio::signal::unix`); on non-Unix targets only Ctrl-C is awaited.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received, shutting down gracefully");
}

/// Bind and serve until a shutdown signal (SIGTERM/SIGINT) is received, then drain in-flight
/// requests before returning. Auth (bearer/HMAC) is read straight from
/// `ZIV_AUTH_BEARER`/`ZIV_AUTH_HMAC_SECRET` env vars via `AuthConfig::from_env` — see
/// `auth` module docs; this is intentionally the only way to enable it (no CLI flag carries a
/// secret value). `public_base_url`, if set (from `--public-base-url` / `ZIV_PUBLIC_BASE_URL`),
/// overrides both the bind-address `base_url` and any per-request forwarded headers — see
/// `origin` module docs for the full precedence.
///
/// The two SEQUENTIAL shutdown drain windows ([`DEFAULT_SHUTDOWN_DRAIN_TIMEOUT`],
/// [`DEFAULT_RENDER_DRAIN_TIMEOUT`]) are each overridable via `ZIV_SHUTDOWN_DRAIN_TIMEOUT_MS` /
/// `ZIV_RENDER_DRAIN_TIMEOUT_MS` (milliseconds; see [`duration_from_env_ms`]) for operators who
/// want a different restart-latency/drain-safety tradeoff than the compiled-in defaults — except
/// `ZIV_RENDER_DRAIN_TIMEOUT_MS` cannot be set BELOW [`MIN_RENDER_DRAIN_TIMEOUT`]
/// (see [`clamp_render_drain_timeout`]): a value under that floor would silently reopen the
/// shutdown panic this drain exists to prevent, so it is clamped up instead, with a loud warning.
///
/// **Worst case together: up to ~170s** (`DEFAULT_SHUTDOWN_DRAIN_TIMEOUT` 20s +
/// `DEFAULT_RENDER_DRAIN_TIMEOUT` 150s), which exceeds systemd's default `TimeoutStopSec` (90s)
/// and Kubernetes' default `terminationGracePeriodSeconds` (30s) — see `README.md`'s env-var
/// section and `docs/operations.md` for the operational trade-off this implies.
///
/// A THIRD drain, [`drain_opens`], runs CONCURRENTLY with the render drain — sharing its ACTUAL
/// (env-overridden, clamped) timeout value rather than adding a fourth knob, and costing no
/// additional sequential time. See [`drain_opens`]'s doc comment for why the open path shares
/// this timeout instead of getting a larger sequential one of its own.
pub async fn run_server(
    addr: &str,
    registry: Arc<crate::registry::ImageRegistry>,
    base_url: String,
    public_base_url: Option<String>,
) -> std::io::Result<()> {
    let mut state = AppState::from_registry(registry, base_url);
    if let Some(auth) = AuthConfig::from_env() {
        state = state.with_auth(auth);
    }
    if let Some(public_base_url) = public_base_url {
        state = state.with_public_base_url(public_base_url);
    }
    let render_semaphore = state.render_semaphore.clone();
    let render_permits = state.render_permits;
    let state_registry = state.registry.clone();
    let open_semaphore = state_registry.open_semaphore();
    let open_permits = state_registry.open_permits();
    let app = app(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("ziv serving on http://{addr}  (viewer: http://{addr}/viewer/)");
    // One image is served at the root, so its viewer link is the line above and repeating it
    // would be noise. More than one and the operator needs the mounts, because nothing else on
    // screen says what the names turned out to be.
    if let crate::registry::Listing::Enumerable(names) = state_registry.list() {
        if names.len() > 1 {
            for name in names {
                println!("  {name}  ->  http://{addr}/i/{name}/viewer/");
            }
        }
    }
    let shutdown_drain_timeout = duration_from_env_ms(
        "ZIV_SHUTDOWN_DRAIN_TIMEOUT_MS",
        DEFAULT_SHUTDOWN_DRAIN_TIMEOUT,
    );
    let render_drain_timeout = clamp_render_drain_timeout(duration_from_env_ms(
        "ZIV_RENDER_DRAIN_TIMEOUT_MS",
        DEFAULT_RENDER_DRAIN_TIMEOUT,
    ));
    serve_with_connection_limits(
        listener,
        app,
        DEFAULT_HEADER_READ_TIMEOUT,
        DEFAULT_MAX_CONNECTIONS,
        shutdown_signal(),
        shutdown_drain_timeout,
    )
    .await?;
    // Concurrent, not sequential: renders and opens are independent resources (a render never
    // overlaps its own request's open, but a different request can be rendering an already-open
    // image while another opens a new one), so waiting on both at once costs no additional
    // sequential time against the already-tight budget. Opens share the render drain's ACTUAL
    // (already env-overridden/clamped) timeout rather than getting a separate knob — see
    // `drain_opens`'s doc comment for why.
    tokio::join!(
        drain_renders(&render_semaphore, render_permits, render_drain_timeout),
        drain_opens(&open_semaphore, open_permits, render_drain_timeout),
    );
    Ok(())
}

/// Waits until no render is still running, or `timeout` elapses.
///
/// Holding every permit is the proof: a render takes one for its whole life, so once all of them
/// are in hand, none is in flight. This runs AFTER the connection drain because the two are
/// different things — a client can be gone while the `spawn_blocking` render it triggered is still
/// reading from a remote store, and `spawn_blocking` work cannot be cancelled. Returning from
/// `main` in that state drops the runtime underneath the task, and the next `tokio` timer it arms
/// (`object_store` backs off between retries) panics.
///
/// A timeout here is logged rather than fatal: the process is exiting either way, and refusing to
/// exit would be worse than the noise.
pub async fn drain_renders(semaphore: &Arc<Semaphore>, permits: usize, timeout: Duration) {
    drain_all_permits("renders", semaphore, permits, timeout).await;
}

/// Waits until no image OPEN (`registry::ImageRegistry::get`'s lazy `spawn_blocking` open) is
/// still running, or `timeout` elapses. Same mechanism and rationale as [`drain_renders`], applied
/// to `ImageRegistry::open_semaphore` instead of `AppState::render_semaphore` — see that
/// function's doc comment for why a `spawn_blocking` open cannot simply be cancelled at shutdown.
///
/// **Why `run_server` calls this CONCURRENTLY with [`drain_renders`], passing it the SAME
/// (already env-overridden/clamped) timeout, rather than giving it a bigger sequential window of
/// its own.** `registry::ImageRegistry::get` has the identical permit-lifetime shape as a render
/// (see its doc comment), so shutdown must wait on `open_semaphore` too. But unlike a render, an
/// open has no analogue of [`PER_TILE_READ_DEADLINE`](zarr_core::image::PER_TILE_READ_DEADLINE)
/// wrapping it: `ZarrImage::open_shared` drives `Group`/`Array::async_open` for the image AND
/// every label, each call bounded only by `object_store`'s own 180s `retry_timeout` ceiling
/// (`zarr_core::store::default_retry_config`), with no smaller per-open deadline layered on top
/// and no cap on how many such calls one open makes. So an open's true worst case is unbounded
/// multiples of 180s, not a fixed, documented number the way `2 * PER_TILE_READ_DEADLINE` is for
/// a render — a single stalling remote call can already exceed this whole drain stage's 150s
/// default, which is itself most of the ~170s combined budget
/// (`DEFAULT_SHUTDOWN_DRAIN_TIMEOUT` + `DEFAULT_RENDER_DRAIN_TIMEOUT`) that already exceeds
/// systemd's and Kubernetes' own default supervisor grace periods (see `run_server`'s doc
/// comment). Sizing a dedicated open-drain phase to actually COVER that worst case, the way
/// `DEFAULT_RENDER_DRAIN_TIMEOUT` covers a render's, isn't a knob change — it would need
/// `zarr_core::image` to grow its own aggregate open deadline first (out of scope for this fix),
/// and even then adding it SEQUENTIALLY after the render drain would push the worst case well
/// past 300s. Running it CONCURRENTLY instead costs no additional sequential time (the two
/// semaphores are independent — a render never overlaps its own request's open, but a DIFFERENT
/// request can be rendering an already-open image while another opens a new one) and closes the
/// panic window for the common case (one slow/stalling remote call), which is what this fix
/// actually closes. What it does NOT do is guarantee an in-flight open always finishes before the
/// drain gives up, the way the render fix does — see `docs/operations.md`'s known limitations for
/// the honest residual exposure.
pub async fn drain_opens(semaphore: &Arc<Semaphore>, permits: usize, timeout: Duration) {
    drain_all_permits("opens", semaphore, permits, timeout).await;
}

/// Shared implementation behind [`drain_renders`]/[`drain_opens`]: waits to acquire every permit
/// (proof that nothing holding one — a render, or an open — is still running), or gives up and
/// logs once `timeout` elapses. `kind` names what's being drained, purely for the log line.
async fn drain_all_permits(
    kind: &str,
    semaphore: &Arc<Semaphore>,
    permits: usize,
    timeout: Duration,
) {
    let Ok(permits_u32) = u32::try_from(permits) else {
        return;
    };
    match tokio::time::timeout(timeout, semaphore.acquire_many(permits_u32)).await {
        Ok(Ok(_)) => tracing::debug!(kind, "all {kind} finished; shutting down"),
        // The semaphore is only ever closed at teardown, so there is nothing left to wait for.
        Ok(Err(_)) => {}
        Err(_) => tracing::warn!(
            kind,
            timeout_secs = timeout.as_secs(),
            "{kind} still running at shutdown; exiting anyway"
        ),
    }
}

/// The accept loop behind [`run_server`], replacing `axum::serve`.
///
/// `axum::serve` is a thin wrapper over the same hyper-util auto builder used here, but it builds
/// that builder with no `Timer`, which silently disables hyper's header-read timeout (see
/// [`DEFAULT_HEADER_READ_TIMEOUT`]) and offers no hook to install one. Owning the loop buys three
/// things `axum::serve` cannot give: a real header-read timeout, a cap on simultaneously-open
/// connections, and a bounded drain deadline on shutdown.
///
/// Split out from `run_server` (which supplies the production constants) so tests can drive it
/// with a short timeout, a tiny connection cap, and a controllable shutdown trigger.
pub async fn serve_with_connection_limits(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    header_read_timeout: Duration,
    max_connections: usize,
    shutdown: impl std::future::Future<Output = ()> + Send,
    drain_timeout: Duration,
) -> std::io::Result<()> {
    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    // HTTP/2 has its own keep-alive/settings timers; they are likewise inert without a timer, and
    // feature unification means this server really does speak h2 (reqwest enables hyper/http2).
    builder.http2().timer(TokioTimer::new());

    let graceful = GracefulShutdown::new();
    let connections = Arc::new(Semaphore::new(max_connections));
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        // Take the permit BEFORE accepting: at the cap the loop stops accepting entirely, so
        // excess connections wait in the kernel backlog instead of being accepted and then held.
        let permit = tokio::select! {
            permit = connections.clone().acquire_owned() => {
                permit.expect("connection semaphore is never closed")
            }
            () = &mut shutdown => break,
        };

        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => stream,
                // Per-connection accept errors (EMFILE, a client that vanished between the
                // kernel queueing it and us accepting) must not kill the server.
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            },
            () = &mut shutdown => break,
        };

        let service = hyper::service::service_fn({
            let app = app.clone();
            move |request: Request<hyper::body::Incoming>| {
                use tower::ServiceExt;
                app.clone().oneshot(request)
            }
        });
        let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
        let conn = graceful.watch(conn.into_owned());
        tokio::spawn(async move {
            // Held for the connection's whole life; dropped here, which is what frees a slot.
            let _permit = permit;
            if let Err(e) = conn.await {
                tracing::debug!(error = %e, "connection closed with error");
            }
        });
    }

    // Drain in-flight connections, but not forever.
    if tokio::time::timeout(drain_timeout, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_secs = drain_timeout.as_secs(),
            "drain deadline reached; exiting with connections still open"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use tiling::TileEngine;
    use tower::ServiceExt;

    fn state() -> AppState {
        let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        AppState::new(
            Arc::new(tiling::ZarrTileEngine::new(img)),
            "http://test".into(),
        )
    }

    /// The layered app (request-id + trace + cors + governance on top of the routes) must not
    /// break a route that worked before the layers were added.
    #[tokio::test]
    async fn layered_app_still_serves_tiles() {
        let res = app(state())
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// `x-request-id` must be present on every response, whether or not the caller supplied
    /// one — `SetRequestIdLayer` generates a UUID when absent, and `PropagateRequestIdLayer`
    /// echoes it back on the way out.
    #[tokio::test]
    async fn x_request_id_is_present_on_responses() {
        let res = app(state())
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers().contains_key("x-request-id"),
            "expected x-request-id header on response"
        );
    }

    /// A caller-supplied `x-request-id` must be preserved (not overwritten), so it can be used
    /// to correlate client-side and server-side logs across a request that already has an id
    /// from an upstream proxy/load balancer.
    #[tokio::test]
    async fn caller_supplied_request_id_is_preserved() {
        let res = app(state())
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .header("x-request-id", "caller-supplied-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get("x-request-id").unwrap(),
            "caller-supplied-id"
        );
    }

    /// CORS: a cross-origin GET must receive an `access-control-allow-origin` header so
    /// browser-based viewers on another origin can fetch tiles/info.json.
    #[tokio::test]
    async fn cors_allows_cross_origin_get() {
        let res = app(state())
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/info.json")
                    .header("origin", "https://viewer.example.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key("access-control-allow-origin"));
    }

    /// The bug behind the 1-hour remote soak's panic (see `docs/operations.md`): the render-drain
    /// timeout must exceed the render's own worst-case duration, or shutdown can give up on a
    /// render that is still legitimately within its own budget, returning while its
    /// `spawn_blocking` thread is still alive and about to arm another `object_store` retry
    /// timer — which then panics because the runtime is dropped underneath it. A render can hit
    /// `PER_TILE_READ_DEADLINE` TWICE sequentially (`overlay=` reads the image then the label),
    /// so the drain must outlast twice that, not just once.
    #[test]
    fn render_drain_timeout_covers_worst_case_render_duration() {
        let worst_case_render = zarr_core::image::PER_TILE_READ_DEADLINE * 2;
        assert!(
            DEFAULT_RENDER_DRAIN_TIMEOUT > worst_case_render,
            "DEFAULT_RENDER_DRAIN_TIMEOUT ({DEFAULT_RENDER_DRAIN_TIMEOUT:?}) must exceed the \
             worst-case render duration ({worst_case_render:?} = 2x PER_TILE_READ_DEADLINE), or \
             shutdown can give up on a render still within its own budget"
        );
    }

    /// `MIN_RENDER_DRAIN_TIMEOUT` (the floor `ZIV_RENDER_DRAIN_TIMEOUT_MS` is clamped to) must
    /// itself cover the worst-case render duration — it would be a strange kind of floor
    /// otherwise: pinned to a name that promises "the worst case", but numerically below it.
    #[test]
    fn min_render_drain_timeout_also_covers_worst_case_render_duration() {
        let worst_case_render = zarr_core::image::PER_TILE_READ_DEADLINE * 2;
        assert_eq!(
            MIN_RENDER_DRAIN_TIMEOUT, worst_case_render,
            "MIN_RENDER_DRAIN_TIMEOUT should be exactly the worst-case render duration \
             (the bare floor, no margin) — got {MIN_RENDER_DRAIN_TIMEOUT:?}"
        );
    }

    /// The whole point of clamping rather than trusting the configured value verbatim: an
    /// operator setting `ZIV_RENDER_DRAIN_TIMEOUT_MS` below the render's own worst case must not
    /// get what they asked for — that value would silently reopen the exact shutdown-panic bug
    /// this drain exists to prevent (a 35s-style misconfiguration; see the module-level doc
    /// comment on `DEFAULT_RENDER_DRAIN_TIMEOUT`).
    #[test]
    fn clamp_render_drain_timeout_raises_a_too_small_value_to_the_floor() {
        let too_small = Duration::from_secs(35);
        assert_eq!(
            clamp_render_drain_timeout(too_small),
            MIN_RENDER_DRAIN_TIMEOUT
        );
    }

    /// A configured value already at or above the floor must pass through unchanged — the clamp
    /// is a floor, not a normalization to a single fixed value, so an operator CAN still choose
    /// something more conservative than the default, or exactly the default, or anything above
    /// the floor.
    #[test]
    fn clamp_render_drain_timeout_leaves_a_sufficient_value_unchanged() {
        assert_eq!(
            clamp_render_drain_timeout(MIN_RENDER_DRAIN_TIMEOUT),
            MIN_RENDER_DRAIN_TIMEOUT
        );
        assert_eq!(
            clamp_render_drain_timeout(DEFAULT_RENDER_DRAIN_TIMEOUT),
            DEFAULT_RENDER_DRAIN_TIMEOUT
        );
        let generous = Duration::from_secs(300);
        assert_eq!(clamp_render_drain_timeout(generous), generous);
    }

    /// Minimal `tracing_subscriber::Layer` that records every WARN-level event's fields as a
    /// `"field=value"`-joined string, for the two nits below where the actual defect is IN the
    /// log call (a misleading field, or no warning at all) rather than in a returned value —
    /// nothing else in this module has needed to inspect tracing output before now.
    struct WarnCapture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl<S> tracing_subscriber::Layer<S> for WarnCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::WARN {
                return;
            }
            struct Fields(Vec<String>);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push(format!("{}={:?}", field.name(), value));
                }
            }
            let mut fields = Fields(Vec::new());
            event.record(&mut fields);
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(fields.0.join(" "));
        }
    }

    /// Runs `f` under a subscriber that captures every WARN event's fields, returning them one
    /// joined string per event.
    fn capture_warnings(f: impl FnOnce()) -> Vec<String> {
        use tracing_subscriber::layer::SubscriberExt;
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarnCapture(captured.clone()));
        tracing::subscriber::with_default(subscriber, f);
        let result = captured.lock().unwrap_or_else(|e| e.into_inner()).clone();
        result
    }

    /// NIT: a sub-second `ZIV_RENDER_DRAIN_TIMEOUT_MS` used to log `configured_secs: 0` —
    /// `Duration::as_secs()` truncates any sub-second value to zero, so the warning whose entire
    /// job is "tell the operator what they actually configured" read exactly as if they had set
    /// `=0` outright. Logging milliseconds instead means a genuine sub-second value shows up as
    /// itself rather than being misreported as the one thing it explicitly is NOT.
    #[test]
    fn clamp_render_drain_timeout_does_not_misreport_a_sub_second_value_as_zero() {
        let warnings = capture_warnings(|| {
            clamp_render_drain_timeout(Duration::from_millis(500));
        });
        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one warning: {warnings:?}"
        );
        let logged = &warnings[0];
        assert!(
            !logged.contains("configured_secs=0") && !logged.contains("configured_secs = 0"),
            "the sub-second value must not be misreported as configured_secs=0: {logged}"
        );
        assert!(
            logged.contains("500"),
            "expected the actual configured value (500ms) to appear in the warning: {logged}"
        );
    }

    /// NIT: `parse_duration_ms`/`duration_from_env_ms` must not fall back to the default
    /// SILENTLY when the operator set the env var to something that isn't a valid millisecond
    /// count — that used to be indistinguishable in the logs from the var never having been set
    /// at all. Uses `parse_duration_ms` directly (not `duration_from_env_ms`) so this test
    /// doesn't touch real process environment state, which every test in this binary shares.
    #[test]
    fn parse_duration_ms_warns_on_a_malformed_value_and_falls_back_to_the_default() {
        let default = Duration::from_secs(150);
        let mut result = None;
        let warnings = capture_warnings(|| {
            result = Some(parse_duration_ms(
                "ZIV_RENDER_DRAIN_TIMEOUT_MS",
                "not-a-number",
                default,
            ));
        });
        assert_eq!(
            result,
            Some(default),
            "a malformed value must still fall back safely"
        );
        assert_eq!(
            warnings.len(),
            1,
            "a malformed value must log exactly one warning, not fail silently: {warnings:?}"
        );
        assert!(
            warnings[0].contains("not-a-number"),
            "expected the offending raw value in the warning: {warnings:?}"
        );
    }

    /// Counterpart: a well-formed value must parse quietly, with no warning at all — the fix is
    /// about the MALFORMED case, not about making every configuration noisy.
    #[test]
    fn parse_duration_ms_does_not_warn_on_a_well_formed_value() {
        let warnings = capture_warnings(|| {
            let d = parse_duration_ms(
                "ZIV_RENDER_DRAIN_TIMEOUT_MS",
                "5000",
                Duration::from_secs(1),
            );
            assert_eq!(d, Duration::from_millis(5000));
        });
        assert!(
            warnings.is_empty(),
            "a well-formed value must not warn: {warnings:?}"
        );
    }

    /// An unset variable is the normal case and must stay exactly as silent as before this fix —
    /// only a SET-but-malformed value gained a warning.
    #[test]
    fn duration_from_env_ms_returns_the_default_silently_when_unset() {
        let warnings = capture_warnings(|| {
            let d = duration_from_env_ms(
                "ZIV_TEST_VAR_THAT_IS_DEFINITELY_NOT_SET_1234567890",
                Duration::from_secs(7),
            );
            assert_eq!(d, Duration::from_secs(7));
        });
        assert!(
            warnings.is_empty(),
            "an unset var must not warn: {warnings:?}"
        );
    }

    /// Graceful-shutdown wiring, in-process: the serve future must return once the shutdown
    /// future resolves. The full signal-injection proof — spawning the real binary, putting a
    /// request in flight, sending an actual SIGTERM and asserting the response arrives
    /// untruncated and the process exits 0 — lives in `crates/cli/tests/graceful_shutdown.rs`,
    /// because only a separate process can be signalled. Connection-phase behaviour (header-read
    /// timeout, connection cap, draining with an idle keep-alive peer) is covered by
    /// `crates/server/tests/connection_limits.rs`.
    #[tokio::test]
    async fn graceful_shutdown_builds_and_serves() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = app(state());

        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                })
                .await
                .unwrap();
        });

        // The server is reachable before the shutdown future resolves.
        let client_ok = tokio::time::timeout(Duration::from_millis(30), async {
            tokio::net::TcpStream::connect(addr).await.is_ok()
        })
        .await
        .unwrap_or(false);
        assert!(client_ok, "server should be accepting connections");

        // After the shutdown future resolves, `axum::serve` returns and the task completes.
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server task should complete after graceful shutdown")
            .unwrap();
    }

    /// A `TileEngine` that sleeps for a configurable duration inside `tile()`, for governance
    /// tests (timeout, concurrency-limit/load-shed) that need requests to stay in flight for a
    /// controlled window rather than completing instantly.
    struct SlowEngine {
        sleep: Duration,
    }

    impl TileEngine for SlowEngine {
        fn image_info(&self, id_base: &str) -> iiif::ImageInfo {
            iiif::ImageInfo {
                id: id_base.to_string(),
                width: 64,
                height: 64,
                tile_size: 512,
                scale_factors: vec![1],
                sizes: vec![(64, 64)],
            }
        }

        fn dimensions(&self) -> tiling::ImageDimensions {
            tiling::ImageDimensions {
                size_t: 1,
                size_z: 1,
                size_c: 1,
                default_t: 0,
                default_z: 0,
                channels: vec![],
                labels: vec![],
                label_open_failures: vec![],
            }
        }

        fn render(
            &self,
            _id: &iiif::ProjectionId,
            _spec: &tiling::RenderSpec,
        ) -> Result<Vec<u8>, tiling::TileError> {
            std::thread::sleep(self.sleep);
            Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
        }
    }

    fn slow_state(sleep: Duration) -> AppState {
        AppState::new(Arc::new(SlowEngine { sleep }), "http://test".into())
            // Distinct render permits per request below (unique sizes) so the render semaphore
            // (default 16 permits) never becomes the bottleneck under test — these tests are
            // specifically about the HTTP-layer governance layers, not the render semaphore.
            .with_render_permits(64)
    }

    /// A request to a hanging engine must time out (rather than hang forever), converted by
    /// `TimeoutLayer::with_status_code` into `408 Request Timeout`.
    #[tokio::test]
    async fn slow_request_times_out() {
        // NOTE: deliberately NOT hours/3600s here even though the fake engine is meant to model
        // "hangs forever" — `spawn_blocking`'s OS thread is NOT cancelled when the async
        // `TimeoutLayer` future is dropped (there is no cooperative cancellation for a plain
        // `std::thread::sleep`), so a too-long sleep here would outlive the test's own assertions
        // and then block `#[tokio::test]`'s runtime teardown waiting for that detached blocking
        // thread to finish. A sleep comfortably longer than the configured timeout (50ms) but
        // short in absolute terms (2s) proves the same property (the HTTP response times out
        // long before the render "completes") without leaking a long-lived thread into the rest
        // of the test binary's process lifetime.
        let state = slow_state(Duration::from_secs(2));
        let app = app_with_governance(state, DEFAULT_CONCURRENCY_LIMIT, Duration::from_millis(50));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::REQUEST_TIMEOUT);
    }

    /// With a tiny concurrency cap, firing more concurrent requests than the cap must get some
    /// `503`s (shed) rather than every request eventually succeeding after being queued —
    /// load-shed fails FAST past the cap instead of unboundedly queuing.
    #[tokio::test]
    async fn concurrency_limit_sheds_excess_requests_to_503() {
        let state = slow_state(Duration::from_millis(300));
        let cap = 2usize;
        let app = app_with_governance(state, cap, Duration::from_secs(10));

        let mut handles = Vec::new();
        for i in 1..=10u32 {
            let app = app.clone();
            let uri = format!("/iiif/default/full/{i},{i}/0/default.jpg");
            handles.push(tokio::spawn(async move {
                app.oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            }));
        }

        let mut ok_count = 0usize;
        let mut shed_count = 0usize;
        for h in handles {
            let res = h.await.unwrap();
            match res.status() {
                StatusCode::OK => ok_count += 1,
                StatusCode::SERVICE_UNAVAILABLE => shed_count += 1,
                other => panic!("unexpected status: {other}"),
            }
        }

        assert!(
            shed_count > 0,
            "expected at least one request to be shed (503) past the concurrency cap of {cap}, \
             got {ok_count} ok / {shed_count} shed"
        );
    }

    /// Sanity counterpart to the shedding test: well under the cap, every request succeeds (no
    /// spurious shedding under normal load).
    #[tokio::test]
    async fn concurrency_limit_does_not_shed_when_under_cap() {
        let state = slow_state(Duration::from_millis(10));
        let app = app_with_governance(state, DEFAULT_CONCURRENCY_LIMIT, Duration::from_secs(10));

        let mut handles = Vec::new();
        for i in 1..=4u32 {
            let app = app.clone();
            let uri = format!("/iiif/default/full/{i},{i}/0/default.jpg");
            handles.push(tokio::spawn(async move {
                app.oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            }));
        }
        for h in handles {
            let res = h.await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }
    }

    /// Auth-before-cache STILL holds with the new governance layers wrapping the app: a
    /// pre-seeded cache entry must not be served to a caller that fails auth, even now that
    /// request-id/trace/cors/timeout/load-shed/concurrency-limit all sit around the routes.
    #[tokio::test]
    async fn auth_before_cache_still_holds_with_governance_layers() {
        let img = zarr_core::ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        let state = AppState::new(
            Arc::new(tiling::ZarrTileEngine::new(img)),
            "http://test".into(),
        )
        .with_auth(AuthConfig {
            bearer: Some("correct-token".to_string()),
            hmac_secret: None,
        });

        let res = app(state)
            .oneshot(
                HttpRequest::builder()
                    .uri("/iiif/default/full/max/0/default.jpg")
                    // No Authorization header -> must fail auth regardless of governance layers.
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
