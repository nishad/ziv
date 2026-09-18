# Running ziv in production

This is the operator's reference for `ziv serve`: every setting, the limits compiled into the
binary, how shutdown behaves behind a supervisor, the security posture, and the known limitations.
The [Operability](../README.md#operability) and [Auth](../README.md#auth-optional) sections of the
README give the overview; this page is the detail behind them.

`ziv export` and `ziv render` are one-shot commands and need none of this. They share only the
[whole-image budget](#limits-compiled-into-the-binary).

## Settings

Secrets are read from the environment only, never from a flag, because argv is visible to other
processes on the host and lands in shell history.

| Setting | Default | Effect |
| --- | --- | --- |
| `ZIV_PUBLIC_BASE_URL` or `--public-base-url` | unset | The address clients use. Written into every `info.json` `id` and into signed URLs. Set it behind a TLS-terminating proxy that does not send `X-Forwarded-Proto`/`X-Forwarded-Host`. |
| `ZIV_AUTH_BEARER` | unset | A static bearer token, compared in constant time. |
| `ZIV_AUTH_HMAC_SECRET` | unset | Enables HMAC-signed URLs (`?exp=<unix seconds>&sig=<…>`). The signature binds the identifier, region and size, not just the path. |
| `--require-auth` | off | Refuse to start unless at least one auth mechanism is configured, instead of serving unauthenticated. |
| `ZIV_ALLOW_INTERNAL_HOSTS` or `--allow-internal-hosts` | off | Disables the SSRF guard for `http(s)://` stores. See [Security posture](#security-posture) before setting it. |
| `ZIV_CHUNK_CACHE_BYTES` | 268435456 (256 MiB) | Byte budget for decoded chunks on the remote read path. |
| `ZIV_SHUTDOWN_DRAIN_TIMEOUT_MS` | 20000 | How long shutdown waits for open connections. |
| `ZIV_RENDER_DRAIN_TIMEOUT_MS` | 150000, floor 120000 | How long shutdown waits for renders still running, and for image opens. A lower value is raised to the floor with a warning. |

When both auth mechanisms are set, a request satisfying either one is accepted. A malformed
millisecond value logs a warning naming the variable and falls back to its default.

## Limits compiled into the binary

| Limit | Value | Why |
| --- | --- | --- |
| Concurrent requests | 512, then load-shed to `503` | Rejecting early is cheaper than queueing work that will time out. |
| Whole-request timeout | 30 s | |
| Header-read timeout | 15 s | A client that opens a connection and never finishes its headers is dropped, rather than holding the connection indefinitely. |
| Open connections | 1024 | |
| Concurrent renders | 16 | Bounds decode and resample memory, and blocking-pool threads, regardless of request rate. |
| Remote read deadline per tile | 60 s | Covers every chunk a tile needs, including retries. |
| Tile cache | 512 MiB of encoded tiles | Bounded by bytes, not entry count, since tile size varies widely. |
| Whole-image budget | 64 megapixels, 65 535 px per edge | Applies to a server's `full/max`, to `ziv export`'s whole-image sizes and to `ziv render`. A request over it is refused with the largest size that would fit. |

## Shutdown behind a supervisor

On `SIGINT` or `SIGTERM`, `ziv serve` drains in two sequential stages: open connections
(`ZIV_SHUTDOWN_DRAIN_TIMEOUT_MS`, 20 s), then renders still running on the blocking pool
(`ZIV_RENDER_DRAIN_TIMEOUT_MS`, 150 s). Image opens in progress are drained at the same time as the
renders, sharing that timeout, so they add no sequential time.

The render stage exists because a render can outlive the request that started it. A remote read
cannot be cancelled once it has started, and stopping the runtime underneath one used to panic.
The floor of 120 s is a render's documented worst case: an `overlay=` request reads the image and
then the label, each bounded by the 60 s read deadline. Below the floor, shutdown could abandon a
render that was still inside its own budget.

**The worst case is about 170 s, which is longer than the common supervisor defaults.** systemd's
`TimeoutStopSec` defaults to 90 s and Kubernetes' `terminationGracePeriodSeconds` to 30 s. On
those defaults ziv is killed mid-drain, and the work it was waiting for is lost exactly as if it
had been stopped abruptly. Lowering `ZIV_RENDER_DRAIN_TIMEOUT_MS` to its floor still leaves a 140 s
worst case, so raise the supervisor's grace period instead:

```ini
# systemd unit
[Service]
TimeoutStopSec=180
```

```yaml
# Kubernetes pod spec
spec:
  terminationGracePeriodSeconds: 180
```

Most shutdowns are far faster than this: the worst case needs a slow render against a remote store
that is failing at the moment of shutdown.

## Security posture

- **SSRF guard.** A remote store's host is resolved and every resulting address is checked against
  loopback, link-local, private and unique-local ranges, including IPv4-mapped IPv6. Redirects are
  disabled, so a permitted host cannot bounce a read to a blocked one.
- **DNS rebinding is a residual risk, not a closed one.** The guard checks at connect-setup time,
  and a host that answers a later lookup with a blocked address could slip past it. ziv never
  builds a store address from request input, since sources are operator-provided, so this is only
  reachable if you configure an untrusted store address. Where store addresses are not fully
  trusted, run ziv behind an egress firewall.
- **Decompression bombs.** Each chunk's decoded size is capped from its trusted metadata before it
  is decoded.
- **No internal detail in errors.** A `500` response carries a generic message; the detail is
  logged. Remote URLs in ziv's own logs are reduced to scheme and host.
- **Dependency advisories.** The accepted ones, and the reason each is not reachable in ziv, are
  listed in `deny.toml`, which CI enforces with `cargo-deny` and `cargo-audit`.

## Monitoring

- `GET /healthz`: liveness, always `200`.
- `GET /readyz`: `200` once the image is open and usable, `503` otherwise.
- `GET /metrics`: Prometheus format. Request rate and latency by route, cache hits and misses, a
  render-concurrency gauge, and image opens by outcome.

All three are unauthenticated and uncached. Alert on `ziv_image_opens_total{outcome="error"}`: a
failing backing store shows up there before it shows up anywhere else.

## Known limitations

- **Memory is bounded but not small.** In a one-hour soak serving a 19 120 × 13 350 image from a
  remote store, resident memory plateaued between 3.6 GB and 6.3 GB with no upward trend, and open
  file descriptors stayed flat. The two caches account for 768 MiB of that; the rest is decode and
  render working memory, which the render limit bounds but which grows with image and tile size.
  Plan for several GB when serving large remote images.
- **Image-open draining is best-effort.** A render has a proven worst-case duration and the drain
  covers it. An image open does not: it is bounded only by the object store client's own retry
  ceiling per call, and one open makes several calls. A remote store that stalls during shutdown can
  still outlast the drain and log a panic on exit. Treat it as a backing-store health problem and
  alert on it, rather than relying on shutdown timing.
- **Remote reads are bandwidth-bound.** Channels are read concurrently and decoded chunks are
  cached, but a slow link to a remote store is slow however it is cached.
- **Label colours are keyed by value up to 2^53.** Label values are carried as 64-bit floats, so
  identifiers above 2^53 can share a colour. See [Label images](labels.md).
