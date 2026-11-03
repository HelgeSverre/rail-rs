//! The health sweep against a local server with well-behaved and badly-behaved mirrors.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use axum::{
    Router,
    extract::Path,
    http::{StatusCode, header::LOCATION},
    response::IntoResponse,
    routing::get,
};
use rail::{
    Config,
    balancer::Strategy,
    health::check_all,
    server::AppState,
    store::{Link, Mirror, Store},
};
use tokio::net::TcpListener;
use url::Url;

fn found(to: String) -> impl IntoResponse {
    (StatusCode::FOUND, [(LOCATION, to)])
}

#[tokio::test]
async fn sweep_classifies_mirrors_without_fetching_the_target() -> Result<()> {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&target_hits);
    let app = Router::new()
        .route("/good", get(async || found("/dest".into())))
        .route("/reencoded", get(async || found("/d%65st".into())))
        .route(
            "/interstitial",
            get(async || "Are you sure you want to continue?"),
        )
        .route("/hijacked", get(async || found("/casino".into())))
        .route("/casino", get(async || "jackpot"))
        // The target redirects forever. That's the target's problem, not the mirrors'.
        .route(
            "/dest",
            get(async move || {
                hits.fetch_add(1, Ordering::Relaxed);
                found("/loop/0".into())
            }),
        )
        .route(
            "/loop/{n}",
            get(async |Path(n): Path<u32>| found(format!("/loop/{}", n + 1))),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
    tokio::spawn(async move { axum::serve(listener, app).await });

    let state = AppState::new(
        Config {
            name: "health".into(),
            public_url: Url::parse("http://127.0.0.1:1/")?,
            upstreams: vec![],
            strategy: Strategy::RoundRobin,
            health_interval: Duration::from_secs(3600),
            allow_private_targets: true,
        },
        Store::open(":memory:")?,
    )?;
    let mirror = |name: &str| Mirror::new(name, base.join(name).map(String::from).unwrap());
    state.store.insert(&Link {
        id: "x".into(),
        // A fragment never reaches the server, so mirrors can't (and needn't) preserve it.
        target: base.join("dest#section")?.into(),
        created_at: 0,
        mirrors: ["good", "reencoded", "interstitial", "hijacked"]
            .map(mirror)
            .to_vec(),
    })?;

    check_all(&state).await?;

    let link = state.store.get("x")?.expect("link exists");
    let health = |name: &str| link.mirrors.iter().find(|m| m.upstream == name).unwrap();
    assert!(health("good").healthy, "{:?}", health("good"));
    assert!(health("reencoded").healthy, "{:?}", health("reencoded"));
    assert!(!health("interstitial").healthy);
    assert!(
        health("interstitial")
            .last_error
            .as_deref()
            .unwrap()
            .contains("interstitial")
    );
    assert!(!health("hijacked").healthy);
    assert_eq!(
        target_hits.load(Ordering::Relaxed),
        0,
        "the target was fetched"
    );
    Ok(())
}
