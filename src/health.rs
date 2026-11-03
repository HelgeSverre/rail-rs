//! Periodically walk every mirror's redirect chain and evict the ones that stopped
//! pointing at the target. Each nested layer does this independently, so a deep enough
//! tower politely DDoSes is.gd.

use std::time::Duration;

use anyhow::{Result, anyhow};
use futures_util::{StreamExt, stream};
use tokio::time::{MissedTickBehavior, interval, timeout};
use tracing::{info, warn};

use crate::{
    now,
    server::AppState,
    store::{CheckOutcome, Mirror},
    trace::trace_until,
};
use url::Url;

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

/// How many mirrors are traced at once during a sweep.
const CONCURRENCY: usize = 8;
/// Upper bound on one mirror's trace: 21 hops at the client's 10 s timeout would be minutes.
const TRACE_TIMEOUT: Duration = Duration::from_secs(60);

/// Check every mirror of every link once. One mirror's failure (or a database error recording
/// it) never stops the rest of the sweep.
pub async fn check_all(state: &AppState) -> Result<()> {
    let links = state.store.all()?;
    // Collected rather than lazy: a borrowed iterator chain in a spawned future trips the
    // compiler's `Send` inference.
    let checks: Vec<(&str, &str, &Mirror)> = links
        .iter()
        .flat_map(|link| {
            link.mirrors
                .iter()
                .map(move |mirror| (link.id.as_str(), link.target.as_str(), mirror))
        })
        .collect();
    stream::iter(checks)
        .for_each_concurrent(CONCURRENCY, |(link_id, target, mirror)| async move {
            let outcome = check(state, mirror, target).await;
            match (mirror.healthy, outcome.healthy) {
                (true, false) => warn!(
                    link = link_id,
                    upstream = mirror.upstream,
                    "mirror evicted: {}",
                    outcome.error.as_deref().unwrap_or("unknown")
                ),
                (false, true) => info!(
                    link = link_id,
                    upstream = mirror.upstream,
                    "mirror recovered"
                ),
                _ => {}
            }
            if let Err(e) = state
                .store
                .record_check(link_id, &mirror.upstream, &outcome)
            {
                warn!(
                    link = link_id,
                    upstream = mirror.upstream,
                    "could not record check: {e:#}"
                );
            }
        })
        .await;
    Ok(())
}

async fn check(state: &AppState, mirror: &Mirror, target: &str) -> CheckOutcome {
    let stop = Url::parse(target).ok();
    let traced = timeout(
        TRACE_TIMEOUT,
        trace_until(&state.client, &mirror.short_url, stop.as_ref()),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow!("trace took longer than {TRACE_TIMEOUT:?}")));
    let (healthy, latency_ms, error) = match traced {
        // Reaching the target is all a mirror has to do; what the target does next is not its fault.
        Ok(t) if t.reached || t.visits(target) => (true, t.first_hop_ms(), None),
        Ok(t) if t.gave_up => (
            false,
            t.first_hop_ms(),
            Some("redirect chain exceeds the browser limit".into()),
        ),
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
