//! Which URLs RAIL agrees to shorten. Every target gets fetched by the health checker forever,
//! so a target pointing at the operator's own network would turn RAIL into an SSRF probe.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TargetError {
    #[error("invalid url: {0}")]
    Invalid(String),
    #[error("only http and https URLs can be shortened")]
    Scheme,
    #[error("{0} is not a public address")]
    Private(String),
    #[error("could not resolve {0}")]
    Unresolvable(String),
}

/// Parse and vet a URL someone asked to shorten.
pub async fn validate(raw: &str, allow_private: bool) -> Result<Url, TargetError> {
    let url = Url::parse(raw.trim()).map_err(|e| TargetError::Invalid(e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(TargetError::Scheme);
    }
    if allow_private {
        return Ok(url);
    }
    match url.host() {
        None => Err(TargetError::Invalid("missing host".into())),
        Some(Host::Ipv4(ip)) => public(IpAddr::V4(ip)).map(|()| url.clone()),
        Some(Host::Ipv6(ip)) => public(IpAddr::V6(ip)).map(|()| url.clone()),
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            if domain == "localhost" || domain.ends_with(".localhost") {
                return Err(TargetError::Private(domain));
            }
            let port = url.port_or_known_default().unwrap_or(80);
            let addrs: Vec<_> = tokio::net::lookup_host((domain.as_str(), port))
                .await
                .map_err(|_| TargetError::Unresolvable(domain.clone()))?
                .collect();
            if addrs.is_empty() {
                return Err(TargetError::Unresolvable(domain));
            }
            addrs.iter().try_for_each(|a| public(a.ip()))?;
            Ok(url)
        }
    }
}

fn public(ip: IpAddr) -> Result<(), TargetError> {
    if is_public(ip) {
        Ok(())
    } else {
        Err(TargetError::Private(ip.to_string()))
    }
}

/// Globally routable unicast, roughly. Conservative on purpose: when in doubt, it's private.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT, 100.64.0.0/10
        || (a == 198 && (b == 18 || b == 19)) // benchmarking, 198.18.0.0/15
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8) // documentation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private} is private");
        }
        for public in ["1.1.1.1", "93.184.215.14", "2606:4700:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public} is public");
        }
    }

    #[tokio::test]
    async fn rejects_internal_targets() {
        for url in [
            "http://127.0.0.1:8080/admin",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://internal.localhost:2000/",
            "http://LOCALHOST./",
        ] {
            assert!(
                matches!(validate(url, false).await, Err(TargetError::Private(_))),
                "{url} should be rejected"
            );
        }
        assert_eq!(
            validate("ftp://example.com/", false).await,
            Err(TargetError::Scheme)
        );
    }

    #[tokio::test]
    async fn the_escape_hatch_allows_private_targets() {
        assert!(validate("http://127.0.0.1:8080/", true).await.is_ok());
    }
}
