//! Redirect tracing against hand-rolled misbehaving servers.

use anyhow::Result;
use axum::{
    Router,
    http::{HeaderValue, StatusCode, header::LOCATION},
    response::IntoResponse,
    routing::get,
};
use rail::trace::trace;
use tokio::net::TcpListener;

async fn serve(app: Router) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(base)
}

fn redirect_to(raw: &'static [u8]) -> impl IntoResponse {
    (
        StatusCode::FOUND,
        [(
            LOCATION,
            HeaderValue::from_bytes(raw).expect("obs-text is a valid header value"),
        )],
    )
}

#[tokio::test]
async fn follows_raw_utf8_locations() -> Result<()> {
    let app = Router::new()
        .route("/start", get(async || redirect_to("/café".as_bytes())))
        .route("/caf%C3%A9", get(async || "arrived"));
    let base = serve(app).await?;

    let t = trace(&rail::http_client()?, &format!("{base}/start")).await?;
    assert_eq!(t.redirects, 1, "{t:?}");
    assert_eq!(t.hops[1].status, Some(200), "{t:?}");
    assert!(t.final_url().unwrap().ends_with("/caf%C3%A9"));
    Ok(())
}

#[tokio::test]
async fn an_unparseable_location_ends_the_trace_instead_of_failing_it() -> Result<()> {
    let app = Router::new().route("/start", get(async || redirect_to(b"http://[bad")));
    let base = serve(app).await?;

    let t = trace(&rail::http_client()?, &format!("{base}/start")).await?;
    assert_eq!(t.hops.len(), 2);
    assert!(t.hops[1].error.as_deref().unwrap().contains("unparseable"));
    Ok(())
}
