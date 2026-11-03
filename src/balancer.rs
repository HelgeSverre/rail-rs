//! The load balancer. The whole point. Allegedly.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

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
    /// Round-robin position per link. A shared counter would let interleaved traffic pin a
    /// link to one mirror (two links alternating means each only ever sees even numbers).
    next: Mutex<HashMap<String, usize>>,
}

impl Balancer {
    pub fn new(strategy: Strategy) -> Self {
        Self {
            strategy,
            next: Mutex::default(),
        }
    }

    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// Pick a healthy mirror, or `None` if every one of them is down.
    pub fn pick<'a>(&self, link_id: &str, mirrors: &'a [Mirror]) -> Option<&'a Mirror> {
        let healthy: Vec<&Mirror> = mirrors.iter().filter(|m| m.healthy).collect();
        if healthy.is_empty() {
            return None;
        }
        match self.strategy {
            Strategy::RoundRobin => {
                let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
                let turn = next.entry(link_id.to_owned()).or_default();
                let pick = healthy[*turn % healthy.len()];
                *turn = turn.wrapping_add(1);
                Some(pick)
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
            .map(|_| b.pick("a", &ms).unwrap().upstream.as_str())
            .collect();
        assert_eq!(picks, ["is.gd", "da.gd", "is.gd", "da.gd"]);
    }

    #[test]
    fn round_robin_rotates_each_link_independently() {
        // The interleaving that used to pin link "a": a 2-mirror link alternating with a 3-mirror one.
        let ms = mirrors();
        let two = vec![ms[0].clone(), ms[2].clone()];
        let three: Vec<_> = (0..3)
            .map(|i| Mirror::new(format!("m{i}"), format!("https://m{i}.example/x")))
            .collect();
        let b = Balancer::new(Strategy::RoundRobin);
        let mut a_picks = Vec::new();
        for _ in 0..4 {
            a_picks.push(b.pick("a", &two).unwrap().upstream.clone());
            b.pick("b", &three);
        }
        assert_eq!(a_picks, ["is.gd", "da.gd", "is.gd", "da.gd"]);
    }

    #[test]
    fn fastest_prefers_low_latency() {
        assert_eq!(
            Balancer::new(Strategy::Fastest)
                .pick("a", &mirrors())
                .unwrap()
                .upstream,
            "da.gd"
        );
    }

    #[test]
    fn all_down_means_none() {
        let mut ms = mirrors();
        ms.iter_mut().for_each(|m| m.healthy = false);
        assert!(Balancer::new(Strategy::Random).pick("a", &ms).is_none());
    }
}
