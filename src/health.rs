//! Periodically walk every mirror's redirect chain and evict the ones that stopped
//! pointing at the target. Each nested layer does this independently, so a deep enough
//! tower politely DDoSes is.gd.

use anyhow::Result;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{info, warn};

use crate::{
    now,
    server::AppState,
    store::{CheckOutcome, Mirror},
    trace::trace,
};

pub async fn run(state: std::sync::Arc<AppState>) {
    let mut tick = interval(state.config.health_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if let Err(e) = check_all(&state).await {
            warn!("health sweep failed: {e:#}");
        }
    }
}

pub async fn check_all(state: &AppState) -> Result<()> {
    for link in state.store.all()? {
        for mirror in &link.mirrors {
            let outcome = check(state, mirror, &link.target).await;
            match (mirror.healthy, outcome.healthy) {
                (true, false) => warn!(
                    link = link.id,
                    upstream = mirror.upstream,
                    "mirror evicted: {}",
                    outcome.error.as_deref().unwrap_or("unknown")
                ),
                (false, true) => info!(
                    link = link.id,
                    upstream = mirror.upstream,
                    "mirror recovered"
                ),
                _ => {}
            }
            state
                .store
                .record_check(&link.id, &mirror.upstream, &outcome)?;
        }
    }
    Ok(())
}

async fn check(state: &AppState, mirror: &Mirror, target: &str) -> CheckOutcome {
    let (healthy, latency_ms, error) = match trace(&state.client, &mirror.short_url).await {
        Ok(t) if t.gave_up => (
            false,
            t.first_hop_ms(),
            Some("redirect chain exceeds the browser limit".into()),
        ),
        Ok(t) if t.visits(target) => (true, t.first_hop_ms(), None),
        Ok(t) if t.redirects == 0 => (
            false,
            t.first_hop_ms(),
            Some(format!(
                "answered {} instead of redirecting (interstitial page? expired?)",
                t.hops
                    .last()
                    .and_then(|h| h.status)
                    .map_or_else(|| "nothing".into(), |s| s.to_string())
            )),
        ),
        Ok(t) => (
            false,
            t.first_hop_ms(),
            Some(format!(
                "chain ended at {} without reaching the target (hijacked?)",
                t.final_url().unwrap_or("nowhere")
            )),
        ),
        Err(e) => (false, None, Some(format!("{e:#}"))),
    };
    CheckOutcome {
        healthy,
        latency_ms,
        error,
        checked_at: now(),
    }
}
