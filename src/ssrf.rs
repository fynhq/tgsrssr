//! SSRF guard: URL/private-IP checks and connection-layer DNS guard.

use std::net::{IpAddr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tracing::warn;

pub(crate) const GUARD_RESOLVE_FAILED: &str = "dns resolve failed by SSRF guard";

// Report DNS lookup failure separately from "private": an outage must not count as a blocked target.
#[derive(Debug, PartialEq, Eq)]
pub enum SsrfOutcome {
    Allowed,
    Private,
    LookupFailed,
    InvalidUrl,
}

async fn lookup_classify(host: &str, port: u16) -> (SsrfOutcome, Vec<SocketAddr>) {
    let addr_str = format!("{}:{}", host, port);
    let lookup = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::lookup_host(&addr_str),
    )
    .await;
    match lookup {
        Ok(Ok(iter)) => {
            let addrs: Vec<SocketAddr> = iter.collect();
            (classify_addrs(&addrs), addrs)
        }
        Ok(Err(_)) | Err(_) => (SsrfOutcome::LookupFailed, Vec::new()),
    }
}

fn classify_addrs(addrs: &[SocketAddr]) -> SsrfOutcome {
    if addrs.iter().any(|sa| is_private_ip(&sa.ip())) {
        SsrfOutcome::Private
    } else {
        SsrfOutcome::Allowed
    }
}

pub async fn ssrf_check(url: &str) -> SsrfOutcome {
    let Ok(u) = url::Url::parse(url) else {
        return SsrfOutcome::InvalidUrl;
    };
    if u.scheme() != "http" && u.scheme() != "https" {
        return SsrfOutcome::Private;
    }
    let Some(host) = u.host_str() else {
        return SsrfOutcome::InvalidUrl;
    };
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_private_ip(&ip) {
            SsrfOutcome::Private
        } else {
            SsrfOutcome::Allowed
        };
    }
    let port = u.port_or_known_default().unwrap_or(443);
    lookup_classify(host, port).await.0
}

pub fn ssrf_check_literal(url: &str) -> SsrfOutcome {
    let Ok(u) = url::Url::parse(url) else {
        return SsrfOutcome::InvalidUrl;
    };
    if u.scheme() != "http" && u.scheme() != "https" {
        return SsrfOutcome::Private;
    }
    let Some(raw_host) = u.host_str() else {
        return SsrfOutcome::InvalidUrl;
    };
    let host = raw_host.trim_start_matches('[').trim_end_matches(']');
    let host_no_dot = host.trim_end_matches('.');
    if let Ok(ip) = host_no_dot.parse::<IpAddr>() {
        return if is_private_ip(&ip) {
            SsrfOutcome::Private
        } else {
            SsrfOutcome::Allowed
        };
    }
    let lower = host_no_dot.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || proxy_host_denied(&lower) {
        return SsrfOutcome::Private;
    }
    SsrfOutcome::Allowed
}

pub fn proxy_host_denied(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return true;
    }
    if !host.contains('.') {
        return true;
    }
    const EXACT: &[&str] = &["metadata.google.internal", "metadata.goog", "instance-data"];
    if EXACT.contains(&host.as_str()) {
        return true;
    }
    const SUFFIXES: &[&str] = &[
        ".localhost",
        ".local",
        ".internal",
        ".lan",
        ".corp",
        ".home.arpa",
        ".svc",
        ".cluster.local",
        ".consul",
        ".localdomain",
    ];
    SUFFIXES.iter().any(|suffix| host.ends_with(suffix))
}

// Fail closed on private or failed DNS in the connection-layer resolver; this closes the rebinding window.
#[derive(Debug, Clone, Copy)]
pub struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let (outcome, addrs) = lookup_classify(&host, 0).await;
            match outcome {
                SsrfOutcome::Allowed => {
                    let boxed: Addrs = Box::new(addrs.into_iter());
                    Ok(boxed)
                }
                SsrfOutcome::Private | SsrfOutcome::LookupFailed | SsrfOutcome::InvalidUrl => {
                    warn!(host = %host, "ssrf: connection-layer DNS guard rejected lookup");
                    Err(Box::new(std::io::Error::other(GUARD_RESOLVE_FAILED))
                        as Box<dyn std::error::Error + Send + Sync>)
                }
            }
        })
    }
}

pub fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 127
                || o[0] == 10
                || (o[0] == 172 && o[1] >= 16 && o[1] <= 31)
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 169 && o[1] == 254)
                || o[0] == 0
                || (o[0] == 100 && o[1] >= 64 && o[1] <= 127)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 192 && o[1] == 0 && o[2] == 2)
                || (o[0] == 198 && o[1] == 51 && o[2] == 100)
                || (o[0] == 203 && o[1] == 0 && o[2] == 113)
                || (o[0] == 192 && o[1] == 88 && o[2] == 99)
                || o[0] >= 224
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4() {
                return is_private_ip(&std::net::IpAddr::V4(v4));
            }
            let seg = v6.segments();
            if seg[0] == 0x0064 && seg[1] == 0xff9b {
                if seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0 {
                    let v4 = std::net::Ipv4Addr::new(
                        (seg[6] >> 8) as u8,
                        seg[6] as u8,
                        (seg[7] >> 8) as u8,
                        seg[7] as u8,
                    );
                    return is_private_ip(&std::net::IpAddr::V4(v4));
                }
                return true;
            }
            v6.is_loopback()
                || (seg[0] & 0xfe00) == 0xfc00
                || v6.is_unspecified()
                || (seg[0] & 0xffc0) == 0xfe80
                || (seg[0] & 0xffc0) == 0xfec0
                || (seg[0] & 0xff00) == 0xff00
                || seg[0] == 0x2002
                || seg[0] == 0x2001 && seg[1] == 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn sa(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse::<IpAddr>().unwrap(), port)
    }

    #[test]
    fn classify_all_public_allowed() {
        assert_eq!(
            classify_addrs(&[
                sa("93.184.216.34", 0),
                sa("2606:2800:220:1:248:1893:25c8:1946", 0)
            ]),
            SsrfOutcome::Allowed
        );
    }

    #[test]
    fn classify_any_private_vetoes() {
        assert_eq!(
            classify_addrs(&[sa("93.184.216.34", 0), sa("127.0.0.1", 0)]),
            SsrfOutcome::Private
        );
        assert_eq!(
            classify_addrs(&[sa("169.254.169.254", 0)]),
            SsrfOutcome::Private
        );
        assert_eq!(classify_addrs(&[sa("::1", 0)]), SsrfOutcome::Private);
    }

    #[test]
    fn nat64_embedded_ipv4_and_multicast_blocked() {
        assert!(is_private_ip(&"64:ff9b::a9fe:a9fe".parse().unwrap()));
        assert!(is_private_ip(&"64:ff9b::7f00:1".parse().unwrap()));
        assert!(!is_private_ip(&"64:ff9b::808:808".parse().unwrap()));
        assert!(is_private_ip(&"64:ff9b:1::1".parse().unwrap()));
        assert!(is_private_ip(&"ff02::1".parse().unwrap()));
    }

    #[tokio::test]
    async fn resolver_rejects_loopback_hostname() {
        let res = GuardedResolver
            .resolve(Name::from_str("localhost").unwrap())
            .await;
        let err = res.err().expect("loopback answer must be rejected");
        assert!(
            err.to_string().contains("dns resolve failed"),
            "error text must keep the is_net_error substring, got: {err}"
        );
    }

    #[tokio::test]
    async fn resolver_rejects_private_literal_ip() {
        let res = GuardedResolver
            .resolve(Name::from_str("10.0.0.1").unwrap())
            .await;
        let err = res.err().expect("private literal must be rejected");
        assert!(err.to_string().contains("dns resolve failed"));
    }

    #[tokio::test]
    async fn resolver_never_leaks_private_for_unresolvable_name() {
        match GuardedResolver
            .resolve(Name::from_str("no-such-host.invalid").unwrap())
            .await
        {
            Err(err) => assert!(err.to_string().contains("dns resolve failed")),
            Ok(mut addrs) => assert!(addrs.all(|a| !is_private_ip(&a.ip()))),
        }
    }

    #[test]
    fn literal_proxy_check_blocks_private_ips_and_localhost() {
        assert_eq!(
            ssrf_check_literal("http://127.0.0.1:8081/"),
            SsrfOutcome::Private
        );
        assert_eq!(ssrf_check_literal("http://10.0.0.5/"), SsrfOutcome::Private);
        assert_eq!(
            ssrf_check_literal("http://169.254.169.254/latest"),
            SsrfOutcome::Private
        );
        assert_eq!(ssrf_check_literal("http://127.1/"), SsrfOutcome::Private);
        assert_eq!(
            ssrf_check_literal("http://2130706433/"),
            SsrfOutcome::Private
        );
        assert_eq!(ssrf_check_literal("http://[::1]/"), SsrfOutcome::Private);
        assert_eq!(
            ssrf_check_literal("http://[fe80::1]/"),
            SsrfOutcome::Private
        );
        assert_eq!(
            ssrf_check_literal("http://localhost/"),
            SsrfOutcome::Private
        );
        assert_eq!(
            ssrf_check_literal("http://foo.localhost./"),
            SsrfOutcome::Private
        );
        assert_eq!(
            ssrf_check_literal("https://example.com/feed"),
            SsrfOutcome::Allowed
        );
        assert_eq!(
            ssrf_check_literal("https://[2606:4700::1111]/"),
            SsrfOutcome::Allowed
        );
        assert_eq!(
            ssrf_check_literal("file:///etc/passwd"),
            SsrfOutcome::Private
        );
    }

    #[test]
    fn proxy_denylist_covers_internal_and_metadata_names() {
        for host in [
            "consul",
            "redis",
            "metadata.google.internal",
            "metadata.goog",
            "instance-data",
            "kubernetes.default.svc",
            "api.cluster.local",
            "printer.local",
            "foo.internal",
            "nas.lan",
        ] {
            assert!(proxy_host_denied(host), "{host} should be denied");
        }
        assert!(!proxy_host_denied("example.com"));
        assert!(!proxy_host_denied("cdn.example.com."));
    }

    #[test]
    fn special_use_ipv4_ranges_are_private() {
        for ip in [
            "192.0.0.1",
            "192.0.0.170",
            "192.0.0.171",
            "192.0.2.1",
            "198.51.100.7",
            "203.0.113.9",
            "192.88.99.1",
        ] {
            assert!(
                is_private_ip(&ip.parse().unwrap()),
                "{ip} should be blocked"
            );
        }
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn site_local_ipv6_is_private() {
        assert!(is_private_ip(&"fec0::1".parse().unwrap()));
        assert!(!is_private_ip(&"2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn ssrf_check_rejects_non_http_scheme() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(ssrf_check("file:///etc/passwd").await, SsrfOutcome::Private);
            assert_eq!(ssrf_check("ws://example.com").await, SsrfOutcome::Private);
            assert_ne!(
                ssrf_check("gopher://example.com").await,
                SsrfOutcome::Allowed
            );
            assert_eq!(ssrf_check("not a url").await, SsrfOutcome::InvalidUrl);
        });
    }
}
