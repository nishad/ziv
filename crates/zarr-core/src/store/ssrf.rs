//! SSRF (server-side request forgery) protection for user/config-controlled `http(s)://` store
//! specs.
//!
//! ziv's store spec (`ZarrImage::open(spec)`) is often config- or request-adjacent input (an
//! operator-supplied `--src`/`serve <path>` argument, potentially templated from an upstream
//! request in some deployments). An `http(s)://` spec can point ANYWHERE, including internal
//! infrastructure the ziv process can reach but the outside world shouldn't be able to probe via
//! it — most notoriously cloud metadata endpoints (`169.254.169.254`, reachable from inside AWS
//! EC2/ECS, GCP GCE, Azure VMs and used to mint credentials) but also any other private-network
//! service (an internal admin panel, a Redis instance with no auth, etc).
//!
//! `s3://`/`gs://`/`az://` are deliberately NOT guarded here: those schemes resolve through each
//! cloud SDK's OWN endpoint construction (region-derived AWS endpoints, `*.googleapis.com`,
//! `*.blob.core.windows.net`, ...), not an arbitrary attacker-chosen host — the one exception
//! (a custom S3-compatible endpoint via `AWS_ENDPOINT`, e.g. for a self-hosted MinIO) is exactly
//! the kind of legitimate internal-network use the `--allow-internal-hosts` escape hatch below
//! covers, and that env var is operator-controlled, not request-controlled, so it's an
//! acceptable opt-in gap rather than a default one.
//!
//! ## Approach: resolve-then-check at store-BUILD time
//!
//! The host is resolved to its IP address(es) via `std::net::ToSocketAddrs` (the same resolver
//! path the eventual HTTP client would use) and EVERY resolved IP is checked against the
//! blocklist below — not just the first one. This catches both a directly-IP-addressed spec
//! (`http://169.254.169.254/...`) and a hostname that RESOLVES to a blocked IP (e.g. the GCP
//! metadata hostname `metadata.google.internal`, or a private rebind target), since the
//! blocklist is IP-range based, not hostname-string based.
//!
//! ### TOCTOU limitation (documented, not silently ignored)
//!
//! This check happens ONCE, when the store is built (`ZarrImage::open`, both at server startup
//! for the auto-stretch read and per-request-adjacent config reload if a deployment does that).
//! The actual HTTP requests happen later, inside `object_store`'s own client, which re-resolves
//! the hostname itself. Between this check and each real connection, DNS could change (a
//! "DNS-rebinding" attack: the attacker's DNS server answers a first lookup with a public IP to
//! pass this check, then a later lookup — used by the real connection — returns a blocked
//! internal IP). Fully closing that gap requires a custom low-level connector that resolves once
//! and pins the connection to that resolved IP for every request (or re-validates on every
//! connect) — meaningfully heavier than a builder-time check (it requires layering a custom
//! `hyper`/`reqwest` connector under `object_store`'s HTTP client, which `object_store` does not
//! expose a hook for as of 0.13). The build-time resolve+check implemented here closes the
//! COMMON case — direct-IP metadata access and stable-DNS internal hostnames, which covers the
//! overwhelming majority of real SSRF attempts against this kind of tool — and is the pragmatic
//! production bar for a first hardening pass; a fully TOCTOU-proof connector-level guard is
//! noted as a follow-up, not implemented here.
//!
//! ## Blocked ranges
//!
//! - **Loopback**: IPv4 `127.0.0.0/8`, IPv6 `::1`.
//! - **Link-local** (includes the cloud metadata endpoint `169.254.169.254`): IPv4
//!   `169.254.0.0/16`, IPv6 `fe80::/10`.
//! - **Private/internal**: IPv4 `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`; IPv6
//!   unique-local `fc00::/7`.
//! - **Unspecified/broadcast**: `0.0.0.0`, IPv4 broadcast `255.255.255.255`, IPv6 `::`.
//!
//! ## Escape hatch
//!
//! Off by default (secure by default). An operator serving from a legitimate internal store
//! (e.g. an in-VPC MinIO reachable only at a private IP) sets `ZIV_ALLOW_INTERNAL_HOSTS=1` (wired
//! to the CLI's `--allow-internal-hosts` flag — see `crates/cli/src/main.rs`) to skip this check
//! entirely for that process.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use crate::error::ZarrError;

/// Reads the `ZIV_ALLOW_INTERNAL_HOSTS` escape-hatch env var: any non-empty value other than `0`
/// or `false` (case-insensitive) is treated as enabled, matching common boolean-env-var
/// conventions elsewhere in ziv (see `server::AuthConfig`/`origin` for the sibling `ZIV_*`
/// vars). Unset or empty is disabled (secure by default).
pub fn allow_internal_hosts_from_env() -> bool {
    match std::env::var("ZIV_ALLOW_INTERNAL_HOSTS") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false"
        }
        Err(_) => false,
    }
}

/// True if `ip` falls in a blocked range: loopback, link-local (incl. the 169.254.169.254 cloud
/// metadata endpoint), private/unique-local, or unspecified/broadcast. See the module doc
/// comment for the exact ranges.
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

fn is_blocked_ipv4(ip: &Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_link_local() // 169.254.0.0/16, covers 169.254.169.254
        || ip.is_private() // 10/8, 172.16/12, 192.168/16
        || ip.is_unspecified() // 0.0.0.0
        || ip.is_broadcast() // 255.255.255.255
}

fn is_blocked_ipv6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    // IPv4-mapped IPv6 addresses (::ffff:a.b.c.d) must be checked against the IPv4 ranges too —
    // otherwise `http://[::ffff:169.254.169.254]/` would sail through.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(&v4);
    }
    let segments = ip.segments();
    // fe80::/10 link-local: first 10 bits are 1111111010.
    let is_link_local = (segments[0] & 0xffc0) == 0xfe80;
    // fc00::/7 unique-local: first 7 bits are 1111110.
    let is_unique_local = (segments[0] & 0xfe00) == 0xfc00;
    is_link_local || is_unique_local
}

/// Resolves `host:port` and rejects the host if ANY resolved IP is blocked (see module doc for
/// the resolve-then-check rationale and TOCTOU caveat). `port` is only used to satisfy
/// `ToSocketAddrs` (DNS resolution doesn't depend on it); a dummy port is fine when the caller
/// doesn't otherwise know one.
///
/// `host` accepts a bare hostname/IPv4 literal (`example.com`, `169.254.169.254`) OR a
/// bracketed IPv6 literal (`[::1]`, matching `url::Url::host_str`'s own serialization of an
/// IPv6 host) — brackets are stripped before resolution, since `ToSocketAddrs` (and the system
/// resolver it wraps) expects the bare address form, not the URL-bracketed one.
///
/// When `allow_internal` is true, this is a no-op (`Ok(())`) — the `--allow-internal-hosts` /
/// `ZIV_ALLOW_INTERNAL_HOSTS` escape hatch.
pub fn check_host_allowed(host: &str, port: u16, allow_internal: bool) -> Result<(), ZarrError> {
    if allow_internal {
        return Ok(());
    }
    let bare_host = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);

    // A literal IP address needs no DNS resolution at all — check it directly. This also
    // sidesteps a subtlety of `ToSocketAddrs` for bracketed-IPv6-with-no-resolver-involved
    // inputs and keeps the common case (most SSRF-relevant specs are direct IPs, e.g. the
    // metadata endpoint) resolver-free and deterministic.
    if let Ok(ip) = bare_host.parse::<IpAddr>() {
        return if is_blocked_ip(&ip) {
            Err(ZarrError::BlockedHost {
                host: host.to_string(),
                ip: ip.to_string(),
            })
        } else {
            Ok(())
        };
    }

    let addrs: Vec<SocketAddr> = (bare_host, port)
        .to_socket_addrs()
        .map_err(|e| ZarrError::Open(format!("failed to resolve host {host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(ZarrError::Open(format!(
            "host {host} resolved to no addresses"
        )));
    }
    for addr in &addrs {
        let ip = addr.ip();
        if is_blocked_ip(&ip) {
            return Err(ZarrError::BlockedHost {
                host: host.to_string(),
                ip: ip.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- IP range classification (pure, no DNS) ---

    #[test]
    fn loopback_v4_is_blocked() {
        assert!(is_blocked_ip(&"127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn loopback_v6_is_blocked() {
        assert!(is_blocked_ip(&"::1".parse().unwrap()));
    }

    #[test]
    fn cloud_metadata_endpoint_is_blocked() {
        assert!(is_blocked_ip(&"169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn link_local_v4_range_is_blocked() {
        assert!(is_blocked_ip(&"169.254.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"169.254.255.255".parse().unwrap()));
    }

    #[test]
    fn link_local_v6_is_blocked() {
        assert!(is_blocked_ip(&"fe80::1".parse().unwrap()));
    }

    #[test]
    fn private_10_range_is_blocked() {
        assert!(is_blocked_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"10.255.255.255".parse().unwrap()));
    }

    #[test]
    fn private_172_16_range_is_blocked() {
        assert!(is_blocked_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"172.31.255.255".parse().unwrap()));
    }

    #[test]
    fn private_192_168_range_is_blocked() {
        assert!(is_blocked_ip(&"192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn unique_local_v6_is_blocked() {
        assert!(is_blocked_ip(&"fc00::1".parse().unwrap()));
        assert!(is_blocked_ip(&"fd12:3456:789a::1".parse().unwrap()));
    }

    #[test]
    fn unspecified_and_broadcast_are_blocked() {
        assert!(is_blocked_ip(&"0.0.0.0".parse().unwrap()));
        assert!(is_blocked_ip(&"255.255.255.255".parse().unwrap()));
        assert!(is_blocked_ip(&"::".parse().unwrap()));
    }

    #[test]
    fn ipv4_mapped_ipv6_metadata_endpoint_is_blocked() {
        assert!(is_blocked_ip(&"::ffff:169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn public_v4_is_allowed() {
        assert!(!is_blocked_ip(&"93.184.216.34".parse().unwrap())); // example.com-ish public IP
    }

    #[test]
    fn public_v6_is_allowed() {
        assert!(!is_blocked_ip(
            &"2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()
        ));
    }

    // --- allow_internal_hosts_from_env ---

    /// Guards tests that mutate `ZIV_ALLOW_INTERNAL_HOSTS` — process-global env state, and
    /// `cargo test` runs tests on separate threads by default (mirrors `AZURE_ENV_LOCK` in
    /// `store.rs`'s existing tests).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn allow_internal_hosts_unset_is_false() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: exclusive access guaranteed by ENV_LOCK for the duration of mutation + read.
        unsafe {
            std::env::remove_var("ZIV_ALLOW_INTERNAL_HOSTS");
        }
        assert!(!allow_internal_hosts_from_env());
    }

    #[test]
    fn allow_internal_hosts_1_is_true() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: exclusive access guaranteed by ENV_LOCK.
        unsafe {
            std::env::set_var("ZIV_ALLOW_INTERNAL_HOSTS", "1");
        }
        let result = allow_internal_hosts_from_env();
        unsafe {
            std::env::remove_var("ZIV_ALLOW_INTERNAL_HOSTS");
        }
        assert!(result);
    }

    #[test]
    fn allow_internal_hosts_false_string_is_false() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: exclusive access guaranteed by ENV_LOCK.
        unsafe {
            std::env::set_var("ZIV_ALLOW_INTERNAL_HOSTS", "false");
        }
        let result = allow_internal_hosts_from_env();
        unsafe {
            std::env::remove_var("ZIV_ALLOW_INTERNAL_HOSTS");
        }
        assert!(!result);
    }

    // --- check_host_allowed (resolve + check) ---

    #[test]
    fn check_host_allowed_rejects_direct_loopback_ip() {
        let err = check_host_allowed("127.0.0.1", 80, false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }));
    }

    #[test]
    fn check_host_allowed_rejects_direct_metadata_ip() {
        let err = check_host_allowed("169.254.169.254", 80, false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }));
    }

    #[test]
    fn check_host_allowed_rejects_localhost_hostname() {
        // "localhost" resolves to 127.0.0.1/::1 via the system resolver/hosts file.
        let err = check_host_allowed("localhost", 80, false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }));
    }

    #[test]
    fn check_host_allowed_rejects_private_ip() {
        let err = check_host_allowed("10.0.0.1", 80, false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }));
        let err = check_host_allowed("192.168.1.1", 80, false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }));
    }

    #[test]
    fn check_host_allowed_rejects_ipv6_loopback() {
        let err = check_host_allowed("::1", 80, false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }));
    }

    /// `url::Url::host_str()` serializes an IPv6 host WITH brackets (`[::1]`, matching how it
    /// appears in the URL text) — the exact string `store::build_http` passes through. Must be
    /// rejected the same as the bare form.
    #[test]
    fn check_host_allowed_rejects_bracketed_ipv6_loopback() {
        let err = check_host_allowed("[::1]", 80, false).unwrap_err();
        assert!(matches!(err, ZarrError::BlockedHost { .. }));
    }

    #[test]
    fn check_host_allowed_accepts_public_host() {
        // A direct public IP needs no real DNS lookup — deterministic offline.
        assert!(check_host_allowed("93.184.216.34", 80, false).is_ok());
    }

    #[test]
    fn check_host_allowed_escape_hatch_permits_blocked_hosts() {
        assert!(check_host_allowed("169.254.169.254", 80, true).is_ok());
        assert!(check_host_allowed("127.0.0.1", 80, true).is_ok());
        assert!(check_host_allowed("10.0.0.1", 80, true).is_ok());
    }
}
