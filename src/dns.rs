//! RFC 6761 says `*.localhost` is loopback. Browsers and curl agree; macOS `getaddrinfo` doesn't.

use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// System DNS first (so Docker aliases like `leaf-a.rail.localhost` still reach their container),
/// then loopback for any `.localhost` name the system couldn't resolve.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalhostFallback;

impl Resolve for LocalhostFallback {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let system = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map(Iterator::collect::<Vec<_>>);
            match system {
                Ok(addrs) if !addrs.is_empty() => Ok(Box::new(addrs.into_iter()) as Addrs),
                _ if host == "localhost" || host.ends_with(".localhost") => {
                    let loopback = [
                        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                        SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
                    ];
                    Ok(Box::new(loopback.into_iter()) as Addrs)
                }
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no addresses for {host}"),
                )
                .into()),
                Err(e) => Err(e.into()),
            }
        })
    }
}
