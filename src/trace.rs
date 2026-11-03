//! Walk a redirect chain hop by hop, the way a browser would, minus the patience.

use std::time::Instant;

use anyhow::Result;
use reqwest::{Client, header::LOCATION};
use serde::Serialize;
use url::Url;

/// Chrome and Firefox both give up after 20 redirects. This is RAIL's hard depth limit.
pub const BROWSER_REDIRECT_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct Hop {
    pub url: String,
    /// HTTP status, or `None` if the request itself failed.
    pub status: Option<u16>,
    pub latency_ms: u64,
    /// The RAIL instance that answered, if it was one.
    pub instance: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trace {
    pub hops: Vec<Hop>,
    pub redirects: usize,
    /// Redirects left before a browser bails with ERR_TOO_MANY_REDIRECTS. Negative means it already did.
    pub headroom: i64,
    pub gave_up: bool,
}

impl Trace {
    pub fn final_url(&self) -> Option<&str> {
        self.hops.last().map(|h| h.url.as_str())
    }

    /// Whether the chain passes through `target` at any point.
    pub fn visits(&self, target: &str) -> bool {
        Url::parse(target).is_ok_and(|target| {
            self.hops
                .iter()
                .any(|hop| Url::parse(&hop.url).is_ok_and(|u| u == target))
        })
    }

    /// Latency of the first hop: how fast this particular mirror answers.
    pub fn first_hop_ms(&self) -> Option<u64> {
        self.hops.first().map(|h| h.latency_ms)
    }
}

/// Follow redirects from `start` until something answers without one, or until one past the
/// browser limit. `client` must not follow redirects itself.
pub async fn trace(client: &Client, start: &str) -> Result<Trace> {
    let mut current = Url::parse(start)?;
    let mut hops = Vec::new();
    let mut redirects = 0;

    loop {
        let started = Instant::now();
        let response = client.get(current.clone()).send().await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                hops.push(Hop {
                    url: current.into(),
                    status: None,
                    latency_ms,
                    instance: None,
                    error: Some(e.to_string()),
                });
                break;
            }
        };

        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let location = status
            .is_redirection()
            .then(|| header(LOCATION.as_str()))
            .flatten();
        hops.push(Hop {
            url: current.to_string(),
            status: Some(status.as_u16()),
            latency_ms,
            instance: header("x-rail-instance"),
            error: None,
        });

        let Some(location) = location else { break };
        redirects += 1;
        if redirects > BROWSER_REDIRECT_LIMIT {
            break;
        }
        current = current.join(&location)?;
    }

    let gave_up = redirects > BROWSER_REDIRECT_LIMIT;
    Ok(Trace {
        hops,
        redirects,
        headroom: BROWSER_REDIRECT_LIMIT as i64 - redirects as i64,
        gave_up,
    })
}
