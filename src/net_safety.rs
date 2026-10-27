//! Stream URL validation.
//!
//! Station URLs come from a public database and from playlist files, so by
//! default the player refuses anything that isn't plain http(s) to a public
//! address. Users who run their own Icecast/Snapcast server at home can turn
//! on "Allow local network streams" in Preferences.
//!
//! Note: GStreamer resolves the host again and follows HTTP redirects on its
//! own, so this is a best-effort guard against obviously local targets, not a
//! complete SSRF barrier.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// Split `scheme://authority/...` into (scheme, host, port).
pub fn split_stream_url(url: &str) -> Option<(String, String, u16)> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let close = after_bracket.find(']')?;
        let host = &after_bracket[..close];
        let suffix = &after_bracket[close + 1..];
        let port = if suffix.is_empty() {
            default_port
        } else {
            suffix.strip_prefix(':')?.parse().ok()?
        };
        (host, port)
    } else {
        if authority.matches(':').count() > 1 {
            return None;
        }
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, default_port),
        }
    };
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    Some((scheme, host.to_string(), port))
}

/// Cheap, synchronous check: scheme, syntax and (unless `allow_local`)
/// obviously-local hosts and IP literals.
pub fn is_allowed_stream_url(url: &str, allow_local: bool) -> bool {
    let Some((_, host, _)) = split_stream_url(url) else {
        return false;
    };
    if allow_local {
        return true;
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost"
        || [".localhost", ".local", ".internal", ".lan", ".home.arpa"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
    {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => !is_non_public_ip(ip),
        Err(_) => true,
    }
}

/// True for loopback, private, link-local, CGNAT, multicast, broadcast,
/// documentation/benchmark and other non-globally-routable addresses.
pub fn is_non_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_non_public_v4(ip),
        IpAddr::V6(ip) => is_non_public_v6(ip),
    }
}

fn is_non_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0 // 0.0.0.0/8
        || (a == 100 && (64..128).contains(&b)) // 100.64.0.0/10 CGNAT / Tailscale
        || (a == 198 && (b == 18 || b == 19)) // 198.18.0.0/15 benchmarking
        || a >= 240 // reserved
}

fn is_non_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_non_public_v4(v4);
    }
    let s = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // unique local
        || (s[0] & 0xffc0) == 0xfe80 // link local
        || (s[0..6] == [0, 0, 0, 0, 0, 0]) // IPv4-compatible (deprecated)
        || (s[0] == 0x64 && s[1] == 0xff9b) // NAT64
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCheck {
    Public,
    NonPublic,
    /// DNS failed or timed out — most likely offline.
    Unresolvable,
}

/// Resolve the stream host and make sure every address is public.
pub async fn check_host(host: String, port: u16) -> HostCheck {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_non_public_ip(ip) {
            HostCheck::NonPublic
        } else {
            HostCheck::Public
        };
    }
    let resolved = tokio::time::timeout(
        Duration::from_secs(8),
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await;
    match resolved {
        Ok(Ok(addresses)) => {
            let addresses: Vec<_> = addresses.collect();
            if addresses.is_empty() {
                HostCheck::Unresolvable
            } else if addresses.iter().any(|a| is_non_public_ip(a.ip())) {
                HostCheck::NonPublic
            } else {
                HostCheck::Public
            }
        }
        _ => HostCheck::Unresolvable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_urls_are_limited_to_http_schemes() {
        assert!(is_allowed_stream_url("https://radio.example/stream", false));
        assert!(is_allowed_stream_url("HTTP://radio.example/stream", false));
        assert!(!is_allowed_stream_url("file:///tmp/music.mp3", true));
        assert!(!is_allowed_stream_url("smb://server/share", true));
        assert!(!is_allowed_stream_url("/tmp/music.mp3", true));
        assert!(!is_allowed_stream_url(
            "http://user:password@example.test/stream",
            true
        ));
        assert!(!is_allowed_stream_url(
            "http://example.test:not-a-port/stream",
            true
        ));
    }

    #[test]
    fn local_targets_blocked_unless_allowed() {
        for url in [
            "http://127.0.0.1/stream",
            "http://[fc00::1]/stream",
            "http://192.168.1.10:8000/stream",
            "http://100.64.0.1/s",
            "http://224.0.0.1/s",
            "http://0.1.2.3/s",
            "http://255.255.255.255/s",
            "http://198.18.0.1/s",
            "http://[::7f00:1]/s",
            "http://[::ffff:10.0.0.1]/s",
            "http://mynas.lan:8000/s",
            "http://box.home.arpa/s",
            "http://localhost./s",
        ] {
            assert!(
                !is_allowed_stream_url(url, false),
                "{url} should be blocked"
            );
            assert!(
                is_allowed_stream_url(url, true),
                "{url} should be allowed when local is on"
            );
        }
        assert!(is_allowed_stream_url("http://8.8.8.8/s", false));
        assert!(is_allowed_stream_url("http://[2a00:1450::1]:8000/s", false));
    }

    #[test]
    fn split_parses_ports() {
        assert_eq!(
            split_stream_url("https://a.example/x"),
            Some(("https".into(), "a.example".into(), 443))
        );
        assert_eq!(
            split_stream_url("http://[::1]:8000/x"),
            Some(("http".into(), "::1".into(), 8000))
        );
        assert_eq!(split_stream_url("http://a.example:99999/"), None);
    }
}
