//! The inexpensive links in the Redundant Array of Inexpensive Links.

use std::{fmt, str::FromStr, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use reqwest::Client;
use serde::{Deserialize, Serialize, Serializer};
use url::Url;

/// A service RAIL can hand a URL to and get a shorter (citation needed) URL back.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Upstream {
    IsGd,
    VGd,
    TinyUrl,
    DaGd,
    Ulvis,
    SpooMe,
    RluRu,
    /// Yandex's shortener. Redirects through a Yandex safe-browsing hop before the target.
    ClckRu,
    /// Not a shortener: an HTTP testing service's `redirect-to` endpoint, which 302s to any URL.
    /// Minting is free (the link is built locally); the health checker finds out if it's down.
    RedirectTo(RedirectService),
    /// TinyURL's authenticated API (`RAIL_TINYURL_TOKEN`).
    TinyUrlApi,
    /// A self-hosted YOURLS instance (`RAIL_YOURLS_SIGNATURE`).
    Yourls(Url),
    /// Another RAIL instance. Load balancers all the way down.
    Rail(Url),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RedirectService {
    HttpbinDev,
    PostmanEcho,
}

impl RedirectService {
    // httpbin.org and httpbingo.org used to qualify; they now only redirect to example.com and
    // friends ("Forbidden redirect URL").
    const ALL: [Self; 2] = [Self::HttpbinDev, Self::PostmanEcho];

    fn name(self) -> &'static str {
        match self {
            Self::HttpbinDev => "httpbin.dev",
            Self::PostmanEcho => "postman-echo",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::HttpbinDev => "https://httpbin.dev/redirect-to",
            Self::PostmanEcho => "https://postman-echo.com/redirect-to",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseUpstreamError {
    #[error(
        "unknown upstream {0:?} (expected one of {list}, rail+<url> or yourls+<url>)",
        list = KEYWORDS.join(", ")
    )]
    Unknown(String),
    #[error("invalid upstream URL")]
    BadUrl(#[from] url::ParseError),
    #[error("upstream base URL must be http(s), got {0}")]
    NotHttp(Url),
}

/// Every upstream spelled as a single keyword.
const KEYWORDS: [&str; 11] = [
    "is.gd",
    "v.gd",
    "tinyurl",
    "da.gd",
    "ulvis",
    "spoo.me",
    "rlu.ru",
    "clck.ru",
    "httpbin.dev",
    "postman-echo",
    "tinyurl-api",
];

/// An http(s) base URL whose path ends in `/`, so `join("api/shorten")` appends instead of
/// replacing the last segment.
fn base_url(raw: &str) -> Result<Url, ParseUpstreamError> {
    let mut url = Url::parse(raw)?;
    if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
        return Err(ParseUpstreamError::NotHttp(url));
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

impl FromStr for Upstream {
    type Err = ParseUpstreamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(base) = s.strip_prefix("rail+") {
            return Ok(Self::Rail(base_url(base)?));
        }
        if let Some(base) = s.strip_prefix("yourls+") {
            return Ok(Self::Yourls(base_url(base)?));
        }
        if let Some(service) = RedirectService::ALL.into_iter().find(|r| r.name() == s) {
            return Ok(Self::RedirectTo(service));
        }
        match s {
            "is.gd" => Ok(Self::IsGd),
            "v.gd" => Ok(Self::VGd),
            "tinyurl" => Ok(Self::TinyUrl),
            "da.gd" => Ok(Self::DaGd),
            "ulvis" => Ok(Self::Ulvis),
            "spoo.me" => Ok(Self::SpooMe),
            "rlu.ru" => Ok(Self::RluRu),
            "clck.ru" => Ok(Self::ClckRu),
            "tinyurl-api" => Ok(Self::TinyUrlApi),
            other => Err(ParseUpstreamError::Unknown(other.to_owned())),
        }
    }
}

impl fmt::Display for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IsGd => f.write_str("is.gd"),
            Self::VGd => f.write_str("v.gd"),
            Self::TinyUrl => f.write_str("tinyurl"),
            Self::DaGd => f.write_str("da.gd"),
            Self::Ulvis => f.write_str("ulvis"),
            Self::SpooMe => f.write_str("spoo.me"),
            Self::RluRu => f.write_str("rlu.ru"),
            Self::ClckRu => f.write_str("clck.ru"),
            Self::RedirectTo(service) => f.write_str(service.name()),
            Self::TinyUrlApi => f.write_str("tinyurl-api"),
            Self::Yourls(base) => write!(f, "yourls+{}", base.as_str().trim_end_matches('/')),
            Self::Rail(base) => write!(f, "rail+{}", base.as_str().trim_end_matches('/')),
        }
    }
}

impl Serialize for Upstream {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Secrets for keyed upstreams. Never part of an upstream's name, so `/api/topology` and logs
/// can list upstreams freely.
#[derive(Clone, Default)]
pub struct Credentials {
    pub tinyurl_token: Option<String>,
    pub yourls_signature: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |s: &Option<String>| if s.is_some() { "<set>" } else { "<unset>" };
        f.debug_struct("Credentials")
            .field("tinyurl_token", &set(&self.tinyurl_token))
            .field("yourls_signature", &set(&self.yourls_signature))
            .finish()
    }
}

/// Everything an upstream needs to mint a link.
#[derive(Debug, Clone)]
pub struct ShortenContext {
    pub client: Client,
    /// Ids of the RAIL instances this request already passed through, forwarded to nested
    /// RAILs so a loop terminates even when cycle detection misses it.
    pub chain: Arc<Vec<String>>,
    pub credentials: Arc<Credentials>,
}

/// Comma-separated ids of the RAIL instances a shorten request has already passed through.
pub const X_RAIL_CHAIN: &str = "x-rail-chain";

const TINYURL_API: &str = "https://api.tinyurl.com/create";

#[derive(Debug, Serialize)]
pub struct ShortenRequest<'a> {
    pub url: &'a str,
}

#[derive(Debug, Deserialize)]
struct RailShortenResponse {
    short_url: String,
}

impl Upstream {
    /// Ask this upstream to shorten `target`, returning the short URL it minted.
    pub async fn shorten(&self, ctx: &ShortenContext, target: &str) -> Result<String> {
        let client = &ctx.client;
        let raw = match self {
            Self::IsGd => {
                simple_get(
                    client,
                    "https://is.gd/create.php",
                    &[("format", "simple"), ("url", target)],
                )
                .await?
            }
            Self::VGd => {
                simple_get(
                    client,
                    "https://v.gd/create.php",
                    &[("format", "simple"), ("url", target)],
                )
                .await?
            }
            Self::TinyUrl => {
                simple_get(
                    client,
                    "https://tinyurl.com/api-create.php",
                    &[("url", target)],
                )
                .await?
            }
            Self::DaGd => simple_get(client, "https://da.gd/s", &[("url", target)]).await?,
            Self::Ulvis => {
                simple_get(client, "https://ulvis.net/api.php", &[("url", target)]).await?
            }
            Self::RluRu => {
                simple_get(
                    client,
                    "https://rlu.ru/index.sema",
                    &[("a", "api"), ("link", target)],
                )
                .await?
            }
            Self::ClckRu => simple_get(client, "https://clck.ru/--", &[("url", target)]).await?,
            Self::SpooMe => spoo_me(client, target).await?,
            Self::RedirectTo(service) => {
                Url::parse_with_params(service.endpoint(), [("url", target)])?.into()
            }
            Self::TinyUrlApi => {
                let token = ctx
                    .credentials
                    .tinyurl_token
                    .as_deref()
                    .context("tinyurl-api needs RAIL_TINYURL_TOKEN")?;
                tinyurl_api(client, TINYURL_API, token, target).await?
            }
            Self::Yourls(base) => {
                let signature = ctx
                    .credentials
                    .yourls_signature
                    .as_deref()
                    .context("yourls needs RAIL_YOURLS_SIGNATURE")?;
                yourls(client, base, signature, target).await?
            }
            Self::Rail(base) => {
                client
                    .post(base.join("api/shorten")?)
                    .header(X_RAIL_CHAIN, ctx.chain.join(","))
                    .json(&ShortenRequest { url: target })
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<RailShortenResponse>()
                    .await?
                    .short_url
            }
        };
        let short = raw.trim();
        let parsed = Url::parse(short).with_context(|| {
            format!("{self} answered with something that is not a URL: {short:?}")
        })?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "{self} minted a non-http URL: {parsed}"
        );
        Ok(parsed.into())
    }
}

/// Parse an upstream list from config. `none` and blanks are skipped; duplicates are an error,
/// because every mirror is keyed by its upstream, and so is a keyed upstream without its key.
pub fn parse_all<'a>(
    specs: impl IntoIterator<Item = &'a str>,
    credentials: &Credentials,
) -> Result<Vec<Upstream>> {
    let mut upstreams: Vec<Upstream> = Vec::new();
    for spec in specs.into_iter().map(str::trim) {
        if spec.is_empty() || spec == "none" {
            continue;
        }
        let upstream: Upstream = spec
            .parse()
            .with_context(|| format!("invalid upstream {spec:?}"))?;
        ensure!(
            !upstreams.contains(&upstream),
            "upstream {upstream} is listed more than once"
        );
        match upstream {
            Upstream::TinyUrlApi if credentials.tinyurl_token.is_none() => {
                bail!("upstream tinyurl-api needs RAIL_TINYURL_TOKEN")
            }
            Upstream::Yourls(_) if credentials.yourls_signature.is_none() => {
                bail!("upstream {upstream} needs RAIL_YOURLS_SIGNATURE")
            }
            _ => {}
        }
        upstreams.push(upstream);
    }
    Ok(upstreams)
}

/// The classic shortener API: GET with a query string, receive the short URL as plain text.
async fn simple_get(client: &Client, endpoint: &str, params: &[(&str, &str)]) -> Result<String> {
    let url = Url::parse_with_params(endpoint, params)?;
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// spoo.me's v1 API. (Its legacy `POST /` mints `http://` links with a tighter rate limit.)
async fn spoo_me(client: &Client, target: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Response {
        short_url: String,
    }
    Ok(client
        .post("https://spoo.me/api/v1/shorten")
        .json(&serde_json::json!({ "long_url": target }))
        .send()
        .await?
        .error_for_status()?
        .json::<Response>()
        .await?
        .short_url)
}

async fn tinyurl_api(client: &Client, endpoint: &str, token: &str, target: &str) -> Result<String> {
    // On failure `data` is an empty array rather than null, so it can't be typed up front.
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        data: serde_json::Value,
        #[serde(default)]
        errors: Vec<String>,
    }
    let response: Response = client
        .post(endpoint)
        .bearer_auth(token)
        .json(&serde_json::json!({ "url": target }))
        .send()
        .await?
        .json()
        .await
        .context("tinyurl-api answered with something that is not its JSON")?;
    match response.data["tiny_url"].as_str() {
        Some(tiny_url) if response.errors.is_empty() => Ok(tiny_url.to_owned()),
        _ => bail!("tinyurl-api refused: {}", response.errors.join("; ")),
    }
}

/// YOURLS answers "already shortened" with a failure status *and* the existing short URL, which
/// is exactly what we want, so the JSON body decides, not the HTTP status.
async fn yourls(client: &Client, base: &Url, signature: &str, target: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Response {
        shorturl: Option<String>,
        message: Option<String>,
    }
    let url = Url::parse_with_params(
        base.join("yourls-api.php")?.as_str(),
        [
            ("action", "shorturl"),
            ("format", "json"),
            ("signature", signature),
            ("url", target),
        ],
    )?;
    let response: Response = client
        .get(url)
        .send()
        .await?
        .json()
        .await
        .context("YOURLS answered with something that is not its JSON")?;
    response.shorturl.with_context(|| {
        format!(
            "YOURLS refused: {}",
            response.message.as_deref().unwrap_or("no reason given")
        )
    })
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, extract::Query, http::HeaderMap, routing::get, routing::post};
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn round_trips_through_display() {
        for spec in KEYWORDS.into_iter().chain([
            "rail+http://leaf-a.rail.localhost:8081",
            "yourls+https://sho.rt/y",
        ]) {
            let upstream: Upstream = spec.parse().unwrap();
            assert_eq!(upstream.to_string(), spec);
            assert_eq!(upstream.to_string().parse::<Upstream>().unwrap(), upstream);
        }
    }

    #[test]
    fn base_urls_gain_a_trailing_slash() {
        let Ok(Upstream::Rail(base)) = "rail+http://a:1/prefix".parse() else {
            panic!("parses");
        };
        assert_eq!(
            base.join("api/shorten").unwrap().as_str(),
            "http://a:1/prefix/api/shorten"
        );
    }

    #[test]
    fn rejects_non_http_base_urls() {
        for spec in [
            "rail+mailto:x",
            "rail+rail+http://x",
            "rail+ftp://x",
            "yourls+file:///x",
        ] {
            assert!(
                spec.parse::<Upstream>().is_err(),
                "{spec} should be rejected"
            );
        }
    }

    #[test]
    fn parse_all_rejects_duplicates_and_skips_none() {
        let none = Credentials::default();
        assert_eq!(parse_all(["none", ""], &none).unwrap(), vec![]);
        assert_eq!(parse_all(["is.gd", "v.gd"], &none).unwrap().len(), 2);
        assert!(parse_all(["is.gd", "is.gd"], &none).is_err());
        // The same instance written two ways is still the same instance.
        assert!(parse_all(["rail+http://a:1", "rail+http://a:1/"], &none).is_err());
    }

    #[test]
    fn keyed_upstreams_need_their_keys() {
        let none = Credentials::default();
        assert!(parse_all(["tinyurl-api"], &none).is_err());
        assert!(parse_all(["yourls+https://sho.rt"], &none).is_err());
        let keys = Credentials {
            tinyurl_token: Some("t".into()),
            yourls_signature: Some("s".into()),
        };
        assert!(parse_all(["tinyurl-api", "yourls+https://sho.rt"], &keys).is_ok());
    }

    #[test]
    fn credentials_never_print() {
        let keys = Credentials {
            tinyurl_token: Some("hunter2".into()),
            yourls_signature: None,
        };
        assert!(!format!("{keys:?}").contains("hunter2"));
    }

    #[test]
    fn rejects_unknown_shorteners() {
        assert!(matches!(
            "bit.ly".parse::<Upstream>(),
            Err(ParseUpstreamError::Unknown(_))
        ));
    }

    #[tokio::test]
    async fn redirect_services_build_links_without_a_request() -> Result<()> {
        let ctx = ShortenContext {
            client: Client::new(),
            chain: Arc::default(),
            credentials: Arc::default(),
        };
        let link = Upstream::RedirectTo(RedirectService::PostmanEcho)
            .shorten(&ctx, "https://example.com/a b?c=d&e")
            .await?;
        assert_eq!(
            link,
            "https://postman-echo.com/redirect-to?url=https%3A%2F%2Fexample.com%2Fa+b%3Fc%3Dd%26e"
        );
        Ok(())
    }

    async fn mock(app: Router) -> Result<Url> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
        tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(url)
    }

    #[tokio::test]
    async fn tinyurl_api_sends_the_token_and_reads_tiny_url() -> Result<()> {
        let app = Router::new().route(
            "/create",
            post(async |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer t0k") {
                    return Json(serde_json::json!({ "data": [], "code": 1, "errors": ["Unauthenticated."] }));
                }
                Json(serde_json::json!({
                    "data": { "tiny_url": format!("https://tinyurl.com/for-{}", body["url"].as_str().unwrap().len()) },
                    "code": 0,
                    "errors": []
                }))
            }),
        );
        let endpoint = mock(app).await?.join("create")?;
        let client = Client::new();
        let ok = tinyurl_api(&client, endpoint.as_str(), "t0k", "https://example.com").await?;
        assert_eq!(ok, "https://tinyurl.com/for-19");
        let err = tinyurl_api(&client, endpoint.as_str(), "wrong", "https://example.com")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Unauthenticated"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn yourls_returns_existing_links_and_reports_refusals() -> Result<()> {
        let app = Router::new().route(
            "/y/yourls-api.php",
            get(async |Query(q): Query<HashMap<String, String>>| {
                let body = match (q["signature"].as_str(), q["url"].as_str()) {
                    ("sig", "https://example.com/new") => serde_json::json!({
                        "status": "success", "shorturl": "https://sho.rt/new"
                    }),
                    // YOURLS reports a duplicate as a failure that still carries the link.
                    ("sig", _) => serde_json::json!({
                        "status": "fail", "code": "error:url", "shorturl": "https://sho.rt/old"
                    }),
                    _ => serde_json::json!({ "message": "Please log in", "errorCode": "403" }),
                };
                (axum::http::StatusCode::OK, Json(body))
            }),
        );
        let base = mock(app).await?.join("y/")?;
        let client = Client::new();
        assert_eq!(
            yourls(&client, &base, "sig", "https://example.com/new").await?,
            "https://sho.rt/new"
        );
        assert_eq!(
            yourls(&client, &base, "sig", "https://example.com/dupe").await?,
            "https://sho.rt/old"
        );
        let err = yourls(&client, &base, "bad", "https://example.com/new")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Please log in"), "{err}");
        Ok(())
    }
}
