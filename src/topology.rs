//! Who balances whom. Cycle detection for a graph made of URL shorteners.

use std::collections::{HashSet, VecDeque};

use anyhow::{Result, ensure};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::upstream::Upstream;

/// Bail out of graph walks that look suspiciously like the whole internet.
const MAX_NODES: usize = 64;
/// Upstreams considered per instance. A remote topology can list anything; we don't have to read it all.
const MAX_FANOUT: usize = 32;

/// What `/api/topology` returns: one instance and its direct upstreams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topology {
    /// Random per-process identity. Two URLs that reach the same instance report the same id.
    #[serde(default)]
    pub id: String,
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

/// Topologies are small; anything bigger is a misbehaving (or hostile) upstream.
const MAX_TOPOLOGY_BYTES: usize = 64 * 1024;

pub async fn fetch(client: &Client, base: &Url) -> Result<Topology> {
    let mut response = client
        .get(base.join("api/topology")?)
        .send()
        .await?
        .error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
        ensure!(
            body.len() <= MAX_TOPOLOGY_BYTES,
            "topology from {base} is larger than {MAX_TOPOLOGY_BYTES} bytes"
        );
    }
    Ok(serde_json::from_slice(&body)?)
}

/// Who "me" is, for cycle detection: the configured URL can be spelled many ways
/// (`localhost` vs `127.0.0.1`, a Docker alias, a trailing slash), the id cannot.
#[derive(Debug, Clone, Copy)]
pub struct Identity<'a> {
    pub id: &'a str,
    pub url: &'a Url,
}

impl Identity<'_> {
    fn is(&self, topology: &Topology) -> bool {
        (!topology.id.is_empty() && topology.id == self.id) || topology.public_url == *self.url
    }
}

/// Breadth-first search from `start` for a path back to `me`.
///
/// Returns the path `start -> ... -> node`, where `node` is `me` under whatever name the graph
/// uses for it. Unreachable nodes are skipped; the `X-Rail-Chain` guard on `/api/shorten` stops
/// any cycle this misses.
pub async fn find_cycle<F, Fut>(me: Identity<'_>, start: &Url, mut fetch: F) -> Option<Vec<Url>>
where
    F: FnMut(Url) -> Fut,
    Fut: Future<Output = Result<Topology>>,
{
    let mut queue = VecDeque::from([vec![start.clone()]]);
    let mut seen = HashSet::from([start.clone()]);

    while let Some(path) = queue.pop_front() {
        let node = path.last().expect("paths always start with `start`");
        if node == me.url {
            return Some(path);
        }
        if seen.len() > MAX_NODES {
            break;
        }
        let Ok(topology) = fetch(node.clone()).await else {
            continue;
        };
        if me.is(&topology) {
            return Some(path);
        }
        seen.insert(topology.public_url.clone());
        for next in topology.rail_upstreams().take(MAX_FANOUT) {
            if seen.insert(next.clone()) || next == *me.url {
                let mut extended = path.clone();
                extended.push(next);
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
///
/// At most `MAX_NODES` topologies are fetched in total, however wide the tree is: every node is
/// advertised by a remote instance we may not operate, and each one costs a request.
pub async fn tree(client: &Client, root: Topology) -> TreeNode {
    let mut walk = Walk { client, fetches: 0 };
    walk.node(root, &mut Vec::new()).await
}

struct Walk<'a> {
    client: &'a Client,
    fetches: usize,
}

impl Walk<'_> {
    /// `path` holds the ancestors on the current branch, so a node that is any of them,
    /// under any name, is reported as a cycle.
    async fn node(&mut self, root: Topology, path: &mut Vec<Topology>) -> TreeNode {
        let on_path = |path: &[Topology], url: &Url, id: Option<&str>| {
            path.iter()
                .any(|t| t.public_url == *url || id.is_some_and(|id| !id.is_empty() && t.id == id))
        };
        path.push(root.clone());
        let mut upstreams = Vec::new();
        for spec in root.upstreams.iter().take(MAX_FANOUT) {
            let child = match spec.parse() {
                Ok(Upstream::Rail(url)) if on_path(path, &url, None) => {
                    TreeNode::Cycle { public_url: url }
                }
                Ok(Upstream::Rail(url)) if self.fetches >= MAX_NODES => TreeNode::Unreachable {
                    public_url: url,
                    error: format!("tree walk stopped after {MAX_NODES} instances"),
                },
                Ok(Upstream::Rail(url)) => {
                    self.fetches += 1;
                    match fetch(self.client, &url).await {
                        Ok(t) if on_path(path, &t.public_url, Some(&t.id)) => {
                            TreeNode::Cycle { public_url: url }
                        }
                        Ok(t) => Box::pin(self.node(t, path)).await,
                        Err(e) => TreeNode::Unreachable {
                            public_url: url,
                            error: format!("{e:#}"),
                        },
                    }
                }
                _ => TreeNode::Shortener { name: spec.clone() },
            };
            upstreams.push(child);
        }
        path.pop();

        TreeNode::Rail {
            instance: root.instance,
            public_url: root.public_url,
            upstreams,
        }
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
                    id: format!("id-{node}"),
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
        let me_url = url(me);
        let me_id = format!("id-{me}");
        let me = Identity {
            id: &me_id,
            url: &me_url,
        };
        find_cycle(me, &url(start), fetch).await.map(|p| {
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
    async fn aliases_of_me_are_recognized_by_id() {
        // b points back at a, but under a different name ("a-alias") that serves a's topology.
        let mut g = graph(&[("b", &["a-alias"])]);
        let mut alias = graph(&[("a", &["b"])]).remove(&url("a")).unwrap();
        alias.public_url = url("a-alias");
        g.insert(url("a-alias"), alias);
        assert_eq!(
            search(&g, "a", "b").await,
            Some(vec!["b".into(), "a-alias".into()])
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
