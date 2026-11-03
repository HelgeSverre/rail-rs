//! Offline integration tests: real RAIL instances on loopback, nested inside each other.
//! Leaves have no upstreams (degraded mode), so nothing here touches the internet.

use std::time::Duration;

use anyhow::Result;
use rail::{
    Config,
    balancer::Strategy,
    store::Store,
    trace::{BROWSER_REDIRECT_LIMIT, trace},
    upstream::Upstream,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use url::Url;

async fn bind() -> Result<(TcpListener, Url)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
    Ok((listener, url))
}

fn start(
    listener: TcpListener,
    public_url: Url,
    name: &str,
    upstreams: Vec<Upstream>,
) -> Result<()> {
    let config = Config {
        name: name.into(),
        public_url,
        upstreams,
        strategy: Strategy::RoundRobin,
        health_interval: Duration::from_secs(3600),
        // The test target lives on loopback.
        allow_private_targets: true,
        credentials: Default::default(),
    };
    let store = Store::open(":memory:")?;
    tokio::spawn(rail::serve(config, store, listener));
    Ok(())
}

async fn rail(name: &str, upstreams: Vec<Upstream>) -> Result<Url> {
    let (listener, url) = bind().await?;
    start(listener, url.clone(), name, upstreams)?;
    Ok(url)
}

async fn target() -> Result<String> {
    let (listener, url) = bind().await?;
    let app = axum::Router::new().fallback(async || "you made it");
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(url.join("destination")?.into())
}

async fn shorten(rail: &Url, target: &str) -> Result<Value> {
    Ok(reqwest::Client::new()
        .post(rail.join("api/shorten")?)
        .json(&json!({ "url": target }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

fn no_redirects() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client builds")
}

#[tokio::test]
async fn nested_instances_redirect_through_every_layer() -> Result<()> {
    let target = target().await?;
    let leaf = rail("leaf", vec![]).await?;
    let mid = rail("mid", vec![Upstream::Rail(leaf)]).await?;
    let top = rail("top", vec![Upstream::Rail(mid)]).await?;

    let resp = shorten(&top, &target).await?;
    assert_eq!(resp["degraded"], false);
    assert_eq!(resp["mirrors"].as_array().map(Vec::len), Some(1));

    let t = trace(&no_redirects(), resp["short_url"].as_str().unwrap()).await?;
    let instances: Vec<_> = t
        .hops
        .iter()
        .filter_map(|h| h.instance.as_deref())
        .collect();
    assert_eq!(instances, ["top", "mid", "leaf"]);
    assert_eq!(t.redirects, 3);
    assert!(t.visits(&target));
    assert_eq!(t.final_url(), Some(target.as_str()));
    Ok(())
}

#[tokio::test]
async fn reshortening_the_same_target_reuses_the_link() -> Result<()> {
    let target = target().await?;
    let leaf = rail("leaf", vec![]).await?;
    let first = shorten(&leaf, &target).await?;
    let second = shorten(&leaf, &target).await?;
    assert_eq!(first["id"], second["id"]);
    assert_eq!(second["reused"], true);
    Ok(())
}

#[tokio::test]
async fn concurrent_shortens_of_one_target_make_one_link() -> Result<()> {
    let target = target().await?;
    let leaf = rail("leaf", vec![]).await?;
    let top = rail("top", vec![Upstream::Rail(leaf)]).await?;

    let requests = (0..10).map(|_| shorten(&top, &target));
    let responses = futures_util::future::try_join_all(requests).await?;
    let ids: std::collections::HashSet<_> = responses.iter().map(|r| r["id"].clone()).collect();
    assert_eq!(ids.len(), 1, "{ids:?}");

    let links: Vec<Value> = reqwest::get(top.join("api/links")?).await?.json().await?;
    assert_eq!(links.len(), 1);
    Ok(())
}

#[tokio::test]
async fn cyclic_upstreams_are_refused() -> Result<()> {
    let target = target().await?;
    let (la, a) = bind().await?;
    let (lb, b) = bind().await?;
    start(la, a.clone(), "a", vec![Upstream::Rail(b.clone())])?;
    start(lb, b.clone(), "b", vec![Upstream::Rail(a.clone())])?;

    let resp = shorten(&a, &target).await?;
    assert_eq!(resp["degraded"], true, "a must not use b: {resp}");
    let error = resp["failed"][0]["error"].as_str().unwrap();
    assert!(error.starts_with("cycle:"), "{error}");

    // Degraded links still work, they just skip the redundancy.
    let t = trace(&no_redirects(), resp["short_url"].as_str().unwrap()).await?;
    assert_eq!(t.redirects, 1);
    Ok(())
}

#[tokio::test]
async fn cycles_through_aliases_are_refused() -> Result<()> {
    // a reaches b as `localhost`, b reaches a as `localhost`, but each calls itself 127.0.0.1.
    // Only identity, not URL spelling, can tell these are the same instances.
    let target = target().await?;
    let (la, a) = bind().await?;
    let (lb, b) = bind().await?;
    let alias = |u: &Url| -> Result<Upstream> {
        Ok(Upstream::Rail(Url::parse(&format!(
            "http://localhost:{}/",
            u.port().unwrap()
        ))?))
    };
    start(la, a.clone(), "a", vec![alias(&b)?])?;
    start(lb, b.clone(), "b", vec![alias(&a)?])?;

    let resp = shorten(&a, &target).await?;
    assert_eq!(resp["degraded"], true, "{resp}");
    assert!(
        resp["failed"][0]["error"]
            .as_str()
            .unwrap()
            .starts_with("cycle:")
    );
    Ok(())
}

#[tokio::test]
async fn the_chain_header_stops_loops_detection_missed() -> Result<()> {
    let target = target().await?;
    let leaf = rail("leaf", vec![]).await?;
    let topology: Value = reqwest::get(leaf.join("api/topology")?)
        .await?
        .json()
        .await?;
    let id = topology["id"].as_str().unwrap();

    let client = reqwest::Client::new();
    let looped = client
        .post(leaf.join("api/shorten")?)
        .header("x-rail-chain", format!("someone-else,{id}"))
        .json(&json!({ "url": target }))
        .send()
        .await?;
    assert_eq!(looped.status(), 508);

    let too_deep = client
        .post(leaf.join("api/shorten")?)
        .header(
            "x-rail-chain",
            (0..20).map(|i| i.to_string()).collect::<Vec<_>>().join(","),
        )
        .json(&json!({ "url": target }))
        .send()
        .await?;
    assert_eq!(too_deep.status(), 508);
    Ok(())
}

#[tokio::test]
async fn a_tower_of_21_is_capped_at_the_browser_limit() -> Result<()> {
    // 21 nested instances would need 21 redirects, one more than browsers follow. The bottom
    // floor refuses the 21st hop, the floor above it degrades, and the link stays followable.
    let target = target().await?;
    let mut url = rail("floor-0", vec![]).await?;
    for floor in 1..=BROWSER_REDIRECT_LIMIT {
        url = rail(&format!("floor-{floor}"), vec![Upstream::Rail(url)]).await?;
    }

    let resp = shorten(&url, &target).await?;
    let t = trace(&no_redirects(), resp["short_url"].as_str().unwrap()).await?;
    assert!(!t.gave_up, "{t:?}");
    assert_eq!(t.redirects, BROWSER_REDIRECT_LIMIT);
    assert_eq!(t.headroom, 0);
    assert!(t.visits(&target));
    Ok(())
}

#[tokio::test]
async fn the_tree_endpoint_shows_the_whole_pyramid() -> Result<()> {
    let leaf = rail("leaf", vec![Upstream::IsGd]).await?;
    let top = rail("top", vec![Upstream::Rail(leaf), Upstream::TinyUrl]).await?;
    let tree: Value = reqwest::get(top.join("api/tree")?).await?.json().await?;
    assert_eq!(tree["instance"], "top");
    assert_eq!(tree["upstreams"][0]["instance"], "leaf");
    assert_eq!(tree["upstreams"][0]["upstreams"][0]["name"], "is.gd");
    assert_eq!(tree["upstreams"][1]["name"], "tinyurl");
    Ok(())
}

#[tokio::test]
async fn the_tree_marks_a_self_alias_as_a_cycle() -> Result<()> {
    // An instance that lists itself under another name used to recurse 64 levels deep.
    let (listener, url) = bind().await?;
    let alias = Url::parse(&format!("http://localhost:{}/", url.port().unwrap()))?;
    start(
        listener,
        url.clone(),
        "narcissus",
        vec![Upstream::Rail(alias)],
    )?;

    let tree: Value = reqwest::get(url.join("api/tree")?).await?.json().await?;
    assert_eq!(tree["upstreams"][0]["kind"], "cycle", "{tree}");
    Ok(())
}
