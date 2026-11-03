//! The load balancer. The whole point. Allegedly.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Serialize;

use crate::store::Mirror;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Take turns. Fair to every shortener, including the slow ones.
    #[default]
    RoundRobin,
    /// Chaos.
    Random,
    /// Lowest measured latency from the last health check.
    Fastest,
}

#[derive(Debug, Default)]
pub struct Balancer {
    strategy: Strategy,
    next: AtomicUsize,
}

impl Balancer {
    pub fn new(strategy: Strategy) -> Self {
        Self {
            strategy,
            next: AtomicUsize::new(0),
        }
    }

    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// Pick a healthy mirror, or `None` if every one of them is down.
    pub fn pick<'a>(&self, mirrors: &'a [Mirror]) -> Option<&'a Mirror> {
        let healthy: Vec<&Mirror> = mirrors.iter().filter(|m| m.healthy).collect();
        if healthy.is_empty() {
            return None;
        }
        match self.strategy {
            Strategy::RoundRobin => {
                Some(healthy[self.next.fetch_add(1, Ordering::Relaxed) % healthy.len()])
            }
            Strategy::Random => Some(healthy[rand::random_range(0..healthy.len())]),
            Strategy::Fastest => healthy
                .into_iter()
                .min_by_key(|m| m.latency_ms.unwrap_or(u64::MAX)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirrors() -> Vec<Mirror> {
        let mut down = Mirror::new("v.gd", "https://v.gd/b");
        down.healthy = false;
        let mut slow = Mirror::new("is.gd", "https://is.gd/a");
        slow.latency_ms = Some(900);
        let mut fast = Mirror::new("da.gd", "https://da.gd/c");
        fast.latency_ms = Some(12);
        vec![slow, down, fast]
    }

    #[test]
    fn round_robin_skips_unhealthy_mirrors() {
        let ms = mirrors();
        let b = Balancer::new(Strategy::RoundRobin);
        let picks: Vec<_> = (0..4)
            .map(|_| b.pick(&ms).unwrap().upstream.as_str())
            .collect();
        assert_eq!(picks, ["is.gd", "da.gd", "is.gd", "da.gd"]);
    }

    #[test]
    fn fastest_prefers_low_latency() {
        assert_eq!(
            Balancer::new(Strategy::Fastest)
                .pick(&mirrors())
                .unwrap()
                .upstream,
            "da.gd"
        );
    }

    #[test]
    fn all_down_means_none() {
        let mut ms = mirrors();
        ms.iter_mut().for_each(|m| m.healthy = false);
        assert!(Balancer::new(Strategy::Random).pick(&ms).is_none());
    }
}
