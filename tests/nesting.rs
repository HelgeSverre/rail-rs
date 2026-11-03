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
async fn a_tower_of_21_breaks_the_browser() -> Result<()> {
    let target = target().await?;
    let mut url = rail("floor-0", vec![]).await?;
    for floor in 1..=BROWSER_REDIRECT_LIMIT {
        url = rail(&format!("floor-{floor}"), vec![Upstream::Rail(url)]).await?;
    }

    let resp = shorten(&url, &target).await?;
    let t = trace(&no_redirects(), resp["short_url"].as_str().unwrap()).await?;
    assert!(
        t.gave_up,
        "21 redirects should exceed the limit of {BROWSER_REDIRECT_LIMIT}"
    );
    assert!(t.headroom < 0);
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
