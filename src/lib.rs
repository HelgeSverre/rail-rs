//! RAIL: a Redundant Array of Inexpensive Links.
//!
//! A URL shortener that shortens your URL with a bunch of other URL shorteners and
//! load-balances between them. It is itself a URL shortener, so it can be an upstream of
//! another RAIL. Blazingly fast, memory safe, and completely unnecessary.

pub mod balancer;
pub mod dns;
pub mod health;
pub mod server;
pub mod store;
pub mod target;
pub mod topology;
pub mod trace;
pub mod upstream;

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use tokio::net::TcpListener;
use url::Url;

use crate::{balancer::Strategy, server::AppState, store::Store, upstream::Upstream};

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    /// The base URL other people (and other RAILs) reach this instance at.
    pub public_url: Url,
    pub upstreams: Vec<Upstream>,
    pub strategy: Strategy,
    pub health_interval: Duration,
    /// Accept targets on loopback, private and link-local addresses. Off by default: every
    /// target is fetched by the health checker, so this would make RAIL an SSRF probe.
    pub allow_private_targets: bool,
}

/// Serve RAIL on `listener` until the process dies, with the health checker in the background.
pub async fn serve(config: Config, store: Store, listener: TcpListener) -> Result<()> {
    let state = Arc::new(AppState::new(config, store)?);
    tokio::spawn(health::run(Arc::clone(&state)));
    axum::serve(listener, server::router(state)).await?;
    Ok(())
}

/// The one HTTP client RAIL uses: never follows redirects, because RAIL needs to see every hop.
pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .dns_resolver(Arc::new(dns::LocalhostFallback))
        // Browser-shaped on purpose: some shorteners (v.gd) only redirect curl and show
        // everyone else an interstitial. Health checks must see what humans see.
        .user_agent(concat!(
            "Mozilla/5.0 (compatible; rail-rs/",
            env!("CARGO_PKG_VERSION"),
            "; Redundant Array of Inexpensive Links)"
        ))
        .build()?)
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Seven base62 characters: 3.5 trillion links per instance, times however many instances you nest.
pub(crate) fn new_id() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    (0..7)
        .map(|_| char::from(ALPHABET[rand::random_range(0..ALPHABET.len())]))
        .collect()
}
