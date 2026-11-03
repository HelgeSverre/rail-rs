//! Who balances whom. Cycle detection for a graph made of URL shorteners.

use std::collections::{HashSet, VecDeque};

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::upstream::Upstream;

/// Bail out of graph walks that look suspiciously like the whole internet.
const MAX_NODES: usize = 64;

/// What `/api/topology` returns: one instance and its direct upstreams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topology {
    pub instance: String,
    pub public_url: Url,
    pub upstreams: Vec<String>,
}

impl Topology {
    pub fn rail_upstreams(&self) -> impl Iterator<Item = Url> + '_ {
        self.upstreams.iter().filter_map(|spec| match spec.parse() {
            Ok(Upstream::Rail(url)) => Some(url),
            _ => None,
        })
    }
}

pub async fn fetch(client: &Client, base: &Url) -> Result<Topology> {
    Ok(client
        .get(base.join("api/topology")?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Breadth-first search from `start` for a path back to `me`.
///
/// Returns the path `start -> ... -> me` if `me` is reachable. Unreachable nodes are skipped:
/// a cycle we can't see is a cycle for the health checker to discover the hard way.
pub async fn find_cycle<F, Fut>(me: &Url, start: &Url, mut fetch: F) -> Option<Vec<Url>>
where
    F: FnMut(Url) -> Fut,
    Fut: Future<Output = Result<Topology>>,
{
    if start == me {
        return Some(vec![me.clone()]);
    }
    let mut queue = VecDeque::from([vec![start.clone()]]);
    let mut seen = HashSet::from([start.clone()]);

    while let Some(path) = queue.pop_front() {
        if seen.len() > MAX_NODES {
            break;
        }
        let node = path.last().expect("paths always start with `start`");
        let Ok(topology) = fetch(node.clone()).await else {
            continue;
        };
        for next in topology.rail_upstreams() {
            let found = next == *me;
            if found || seen.insert(next.clone()) {
                let mut extended = path.clone();
                extended.push(next);
                if found {
                    return Some(extended);
                }
                queue.push_back(extended);
            }
        }
    }
    None
}

/// The full balancer tree, as served by `/api/tree`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TreeNode {
    Shortener {
        name: String,
    },
    Rail {
        instance: String,
        public_url: Url,
        upstreams: Vec<TreeNode>,
    },
    Cycle {
        public_url: Url,
    },
    Unreachable {
        public_url: Url,
        error: String,
    },
}

/// Build the tree rooted at a local topology, fetching nested RAIL instances recursively.
pub async fn tree(client: &Client, root: Topology, ancestors: &[Url]) -> TreeNode {
    let mut path = ancestors.to_vec();
    path.push(root.public_url.clone());

    let mut upstreams = Vec::with_capacity(root.upstreams.len());
    for spec in &root.upstreams {
        let child = match spec.parse() {
            Ok(Upstream::Rail(url)) if path.contains(&url) => TreeNode::Cycle { public_url: url },
            Ok(Upstream::Rail(_)) if path.len() >= MAX_NODES => TreeNode::Unreachable {
                public_url: root.public_url.clone(),
                error: "tree too deep".into(),
            },
            Ok(Upstream::Rail(url)) => match fetch(client, &url).await {
                Ok(topology) => Box::pin(tree(client, topology, &path)).await,
                Err(e) => TreeNode::Unreachable {
                    public_url: url,
                    error: format!("{e:#}"),
                },
            },
            _ => TreeNode::Shortener { name: spec.clone() },
        };
        upstreams.push(child);
    }

    TreeNode::Rail {
        instance: root.instance,
        public_url: root.public_url,
        upstreams,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::anyhow;

    use super::*;

    fn url(name: &str) -> Url {
        Url::parse(&format!("http://{name}.rail.localhost/")).unwrap()
    }

    fn graph(edges: &[(&str, &[&str])]) -> HashMap<Url, Topology> {
        edges
            .iter()
            .map(|(node, ups)| {
                let topology = Topology {
                    instance: (*node).into(),
                    public_url: url(node),
                    upstreams: ups
                        .iter()
                        .map(|u| format!("rail+{}", url(u)))
                        .chain(["is.gd".into()])
                        .collect(),
                };
                (url(node), topology)
            })
            .collect()
    }

    async fn search(g: &HashMap<Url, Topology>, me: &str, start: &str) -> Option<Vec<String>> {
        let fetch = |u: Url| std::future::ready(g.get(&u).cloned().ok_or_else(|| anyhow!("down")));
        find_cycle(&url(me), &url(start), fetch).await.map(|p| {
            p.iter()
                .map(|u| {
                    u.host_str()
                        .unwrap()
                        .trim_end_matches(".rail.localhost")
                        .to_owned()
                })
                .collect()
        })
    }

    #[tokio::test]
    async fn finds_indirect_cycles() {
        let g = graph(&[("a", &["b"]), ("b", &["c"]), ("c", &["a"])]);
        assert_eq!(
            search(&g, "a", "b").await,
            Some(vec!["b".into(), "c".into(), "a".into()])
        );
    }

    #[tokio::test]
    async fn diamonds_are_not_cycles() {
        let g = graph(&[
            ("top", &["l", "r"]),
            ("l", &["leaf"]),
            ("r", &["leaf"]),
            ("leaf", &[]),
        ]);
        assert_eq!(search(&g, "top", "l").await, None);
    }

    #[tokio::test]
    async fn self_reference_is_a_cycle() {
        assert_eq!(
            search(&HashMap::new(), "a", "a").await,
            Some(vec!["a".into()])
        );
    }

    #[tokio::test]
    async fn unreachable_nodes_are_skipped() {
        let g = graph(&[("b", &["ghost", "c"]), ("c", &["a"])]);
        assert_eq!(
            search(&g, "a", "b").await,
            Some(vec!["b".into(), "c".into(), "a".into()])
        );
    }
}
