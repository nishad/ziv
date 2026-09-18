//! Public origin resolution for URLs embedded in responses (currently: the IIIF info.json `id`).
//!
//! Behind a TLS-terminating reverse proxy, the address ziv itself binds to (`base_url`, e.g.
//! `http://127.0.0.1:3000`) is almost never the address a client should use to re-request a
//! tile — the proxy terminates TLS and rewrites the host, so the origin ziv sees on its own
//! socket is wrong for a public `@id`. Three sources are considered, in this precedence order:
//!
//! 1. **Explicit config** (`AppState::public_base_url`, set via `--public-base-url` /
//!    `ZIV_PUBLIC_BASE_URL`) — always wins when set. An operator who configures this is stating
//!    the public origin outright; no header from any request should override it.
//! 2. **Forwarded headers** (`X-Forwarded-Proto` + `X-Forwarded-Host`) — honored whenever
//!    present, without a separate opt-in flag. This mirrors how most reverse-proxy-fronted
//!    services behave by default (nginx/Caddy/ALB all set these); the trust consideration is
//!    that these headers are, in general, trivially spoofable by a direct caller that bypasses
//!    the proxy. That's an accepted tradeoff here: the only thing an attacker can influence by
//!    spoofing them is the `id` STRING embedded in a response body they already control the
//!    request for (it does not affect authz, tile bytes, or which image is served) — the
//!    blast radius is "a misleading URL echoed back to the same caller who sent the header",
//!    not a privilege or data boundary. A deployment that wants to rule this out entirely
//!    should terminate TLS at a proxy that also STRIPS/overwrites any client-supplied
//!    `X-Forwarded-*` before forwarding (standard proxy hygiene) and/or set `--public-base-url`
//!    explicitly, which always wins regardless (option 1).
//! 3. **Bind address fallback** (`AppState::base_url`) — used when neither of the above is
//!    present, i.e. an unproxied direct deployment.
use axum::http::HeaderMap;

/// Resolve the public base URL (no trailing slash) for the CURRENT request, applying the
/// precedence documented on this module: explicit config > forwarded headers > bind address.
pub fn resolve_base_url(
    public_base_url: Option<&str>,
    headers: &HeaderMap,
    fallback_base_url: &str,
) -> String {
    if let Some(explicit) = public_base_url {
        return explicit.trim_end_matches('/').to_string();
    }

    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());

    if let (Some(proto), Some(host)) = (proto, host) {
        return format!("{proto}://{host}");
    }

    fallback_base_url.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn falls_back_to_bind_address_when_nothing_else_present() {
        let resolved = resolve_base_url(None, &HeaderMap::new(), "http://127.0.0.1:3000");
        assert_eq!(resolved, "http://127.0.0.1:3000");
    }

    #[test]
    fn honors_forwarded_headers_when_present() {
        let h = headers(&[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "example.org"),
        ]);
        let resolved = resolve_base_url(None, &h, "http://127.0.0.1:3000");
        assert_eq!(resolved, "https://example.org");
    }

    #[test]
    fn explicit_config_wins_over_forwarded_headers() {
        let h = headers(&[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "example.org"),
        ]);
        let resolved = resolve_base_url(
            Some("https://configured.example.net"),
            &h,
            "http://127.0.0.1:3000",
        );
        assert_eq!(resolved, "https://configured.example.net");
    }

    #[test]
    fn partial_forwarded_headers_fall_back() {
        // Only proto, no host -> not enough to construct an origin, falls back.
        let h = headers(&[("x-forwarded-proto", "https")]);
        let resolved = resolve_base_url(None, &h, "http://127.0.0.1:3000");
        assert_eq!(resolved, "http://127.0.0.1:3000");
    }

    #[test]
    fn trims_trailing_slash_from_explicit_config() {
        let resolved = resolve_base_url(
            Some("https://example.org/"),
            &HeaderMap::new(),
            "http://127.0.0.1:3000",
        );
        assert_eq!(resolved, "https://example.org");
    }
}
