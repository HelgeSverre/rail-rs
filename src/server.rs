//! HTTP surface: shorten, redirect, introspect.

use std::{collections::HashMap, sync::Arc};

use anyhow::anyhow;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{
        HeaderName, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION},
    },
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use qrcode::{QrCode, render::svg};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, task::JoinSet};
use tracing::warn;
use url::Url;

use crate::{
    Config,
    balancer::{Balancer, Strategy},
    new_id, now,
    store::{Link, Mirror, Store},
    topology::{self, Topology, TreeNode},
    trace::{Trace, trace},
    upstream::Upstream,
};

pub const X_RAIL_INSTANCE: HeaderName = HeaderName::from_static("x-rail-instance");
pub const X_RAIL_UPSTREAM: HeaderName = HeaderName::from_static("x-rail-upstream");

pub struct AppState {
    pub config: Config,
    pub store: Store,
    /// Never follows redirects: RAIL needs to see every hop.
    pub client: Client,
    pub balancer: Balancer,
    /// Rail upstreams currently excluded because they route back to us, with the offending path.
    pub cyclic: Mutex<HashMap<Upstream, Vec<Url>>>,
}

impl AppState {
    pub fn new(config: Config, store: Store) -> anyhow::Result<Self> {
        let client = crate::http_client()?;
        Ok(Self {
            balancer: Balancer::new(config.strategy),
            config,
            store,
            client,
            cyclic: Mutex::default(),
        })
    }

    fn short_url(&self, id: &str) -> String {
        self.config
            .public_url
            .join(id)
            .map_or_else(|_| id.to_owned(), String::from)
    }

    fn topology(&self) -> Topology {
        Topology {
            instance: self.config.name.clone(),
            public_url: self.config.public_url.clone(),
            upstreams: self
                .config
                .upstreams
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }

    /// Upstreams safe to shorten with right now. Rail upstreams that lead back here are skipped.
    async fn usable_upstreams(&self) -> (Vec<Upstream>, Vec<Failure>) {
        let mut usable = Vec::new();
        let mut skipped = Vec::new();
        let me = &self.config.public_url;
        for upstream in &self.config.upstreams {
            if let Upstream::Rail(base) = upstream {
                let fetch = |u: Url| {
                    let client = self.client.clone();
                    async move { topology::fetch(&client, &u).await }
                };
                if let Some(path) = topology::find_cycle(me, base, fetch).await {
                    let shown = std::iter::once(me)
                        .chain(&path)
                        .map(|u| u.as_str().trim_end_matches('/'))
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    warn!(upstream = %upstream, "skipping cyclic upstream: {shown}");
                    skipped.push(Failure {
                        upstream: upstream.to_string(),
                        error: format!("cycle: {shown}"),
                    });
                    self.cyclic.lock().await.insert(upstream.clone(), path);
                    continue;
                }
                self.cyclic.lock().await.remove(upstream);
            }
            usable.push(upstream.clone());
        }
        (usable, skipped)
    }

    fn response(&self, link: Link, failed: Vec<Failure>, reused: bool) -> ShortenResponse {
        let short_url = self.short_url(&link.id);
        let ratio = short_url.len() as f64 / link.target.len().max(1) as f64;
        ShortenResponse {
            degraded: link.mirrors.is_empty(),
            compression_ratio: (ratio * 100.0).round() / 100.0,
            short_url,
            reused,
            failed,
            link,
        }
    }
}

type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(async || "ok"))
        .route("/api/shorten", post(shorten))
        .route("/api/links", get(list_links))
        .route("/api/topology", get(get_topology))
        .route("/api/tree", get(get_tree))
        .route("/{id}", get(redirect))
        .route("/{id}/info", get(info))
        .route("/{id}/trace", get(trace_link))
        .route("/{id}/qr", get(qr))
        .with_state(state)
}

pub struct AppError {
    status: StatusCode,
    error: anyhow::Error,
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: e.into(),
        }
    }
}

impl AppError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self {
            status,
            error: anyhow!(msg.into()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({ "error": format!("{:#}", self.error) }));
        (self.status, body).into_response()
    }
}

#[derive(Debug, Deserialize)]
pub struct ShortenBody {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct Failure {
    pub upstream: String,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct ShortenResponse {
    pub short_url: String,
    /// Short URL length over target length. Above 1.0 means we made it longer. Congratulations.
    pub compression_ratio: f64,
    /// No upstream succeeded; this instance redirects straight to the target.
    pub degraded: bool,
    /// The target was already shortened before; no shorteners were bothered.
    pub reused: bool,
    pub failed: Vec<Failure>,
    #[serde(flatten)]
    pub link: Link,
}

async fn shorten(
    State(state): State<SharedState>,
    Json(body): Json<ShortenBody>,
) -> Result<Json<ShortenResponse>, AppError> {
    let target = Url::parse(body.url.trim())
        .map_err(|e| AppError::new(StatusCode::BAD_REQUEST, format!("invalid url: {e}")))?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "only http and https URLs can be shortened",
        ));
    }
    let target = String::from(target);

    if let Some(link) = state.store.find_by_target(&target)? {
        return Ok(Json(state.response(link, Vec::new(), true)));
    }

    let (upstreams, mut failed) = state.usable_upstreams().await;
    let mut tasks = JoinSet::new();
    for upstream in upstreams {
        let client = state.client.clone();
        let target = target.clone();
        tasks.spawn(async move {
            let minted = upstream.shorten(&client, &target).await;
            (upstream, minted)
        });
    }

    let mut mirrors = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (upstream, minted) = joined?;
        match minted {
            Ok(short_url) => mirrors.push(Mirror::new(upstream.to_string(), short_url)),
            Err(e) => {
                warn!(upstream = %upstream, "shorten failed: {e:#}");
                failed.push(Failure {
                    upstream: upstream.to_string(),
                    error: format!("{e:#}"),
                });
            }
        }
    }
    mirrors.sort_by(|a, b| a.upstream.cmp(&b.upstream));

    let link = Link {
        id: new_id(),
        target,
        created_at: now(),
        mirrors,
    };
    state.store.insert(&link)?;
    Ok(Json(state.response(link, failed, false)))
}

async fn redirect(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let link = state
        .store
        .get(&id)?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "no such link"))?;
    let (location, upstream) = match state.balancer.pick(&link.mirrors) {
        Some(mirror) => (mirror.short_url.as_str(), mirror.upstream.as_str()),
        None => (
            link.target.as_str(),
            "none (degraded: serving the raw URL like an animal)",
        ),
    };
    // 302, never 301: a cached permanent redirect would bypass the load balancer entirely.
    Ok((
        StatusCode::FOUND,
        [
            (LOCATION, location),
            (CACHE_CONTROL, "no-store"),
            (X_RAIL_INSTANCE, state.config.name.as_str()),
            (X_RAIL_UPSTREAM, upstream),
        ],
    )
        .into_response())
}

async fn info(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<ShortenResponse>, AppError> {
    let link = state
        .store
        .get(&id)?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "no such link"))?;
    Ok(Json(state.response(link, Vec::new(), true)))
}

async fn trace_link(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Trace>, AppError> {
    if state.store.get(&id)?.is_none() {
        return Err(AppError::new(StatusCode::NOT_FOUND, "no such link"));
    }
    Ok(Json(trace(&state.client, &state.short_url(&id)).await?))
}

/// A printed QR code should point at a buffer, not the final URL. This one points at a buffer
/// of buffers of buffers.
async fn qr(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if state.store.get(&id)?.is_none() {
        return Err(AppError::new(StatusCode::NOT_FOUND, "no such link"));
    }
    let svg = QrCode::new(state.short_url(&id).as_bytes())?
        .render::<svg::Color<'_>>()
        .min_dimensions(256, 256)
        .build();
    Ok(([(CONTENT_TYPE, "image/svg+xml")], svg).into_response())
}

async fn list_links(State(state): State<SharedState>) -> Result<Json<Vec<Link>>, AppError> {
    Ok(Json(state.store.all()?))
}

async fn get_topology(State(state): State<SharedState>) -> Json<Topology> {
    Json(state.topology())
}

async fn get_tree(State(state): State<SharedState>) -> Json<TreeNode> {
    Json(topology::tree(&state.client, state.topology(), &[]).await)
}

#[derive(Serialize)]
struct IndexInfo<'a> {
    instance: &'a str,
    strategy: Strategy,
    upstreams: Vec<String>,
}

async fn index(State(state): State<SharedState>) -> Html<String> {
    let info = IndexInfo {
        instance: &state.config.name,
        strategy: state.balancer.strategy(),
        upstreams: state
            .config
            .upstreams
            .iter()
            .map(ToString::to_string)
            .collect(),
    };
    let json = serde_json::to_string(&info).unwrap_or_else(|_| "{}".into());
    Html(include_str!("index.html").replace("/*RAIL_INFO*/null", &json))
}
