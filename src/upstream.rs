//! The inexpensive links in the Redundant Array of Inexpensive Links.

use std::{collections::BTreeMap, fmt, str::FromStr, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};
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
    /// TinyURL's authenticated API.
    TinyUrlApi,
    /// x.gd. Free key, requested by e-mail.
    XGd,
    /// Cuttly. Free plan: 30 links a month, 3 API calls a minute.
    Cuttly,
    /// Short.io. Free plan: 1,000 links on a per-account `*.short.gy` domain.
    ShortIo,
    /// Dub. Free plan: 25 links a month on `dub.sh`.
    Dub,
    /// Rebrandly. Free plan: 10 links a month on `rebrand.ly`.
    Rebrandly,
    /// Bitly. Free-plan links show an ad page before redirecting; paid plans redirect directly.
    Bitly,
    /// T.ly. The API needs a paid plan.
    Tly,
    /// Linkly. Free plan: 100 links a month.
    Linkly,
    /// A self-hosted YOURLS instance.
    Yourls(Url),
    /// A Kutt instance (kutt.to closed sign-ups; self-hosted works).
    Kutt(Url),
    /// A self-hosted Shlink instance.
    Shlink(Url),
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
        "unknown upstream {0:?} (expected one of {list}, or rail+, yourls+, kutt+ or shlink+ followed by a URL)",
        list = KEYWORDS.join(", ")
    )]
    Unknown(String),
    #[error("invalid upstream URL")]
    BadUrl(#[from] url::ParseError),
    #[error("upstream base URL must be http(s), got {0}")]
    NotHttp(Url),
}

/// Every upstream spelled as a single keyword.
const KEYWORDS: [&str; 19] = [
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
    "x.gd",
    "cutt.ly",
    "short.io",
    "dub",
    "rebrandly",
    "bitly",
    "t.ly",
    "linkly",
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
        type Based = fn(Url) -> Upstream;
        let based: [(&str, Based); 4] = [
            ("rail+", Self::Rail),
            ("yourls+", Self::Yourls),
            ("kutt+", Self::Kutt),
            ("shlink+", Self::Shlink),
        ];
        for (prefix, variant) in based {
            if let Some(base) = s.strip_prefix(prefix) {
                return Ok(variant(base_url(base)?));
            }
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
            "x.gd" => Ok(Self::XGd),
            "cutt.ly" => Ok(Self::Cuttly),
            "short.io" => Ok(Self::ShortIo),
            "dub" => Ok(Self::Dub),
            "rebrandly" => Ok(Self::Rebrandly),
            "bitly" => Ok(Self::Bitly),
            "t.ly" => Ok(Self::Tly),
            "linkly" => Ok(Self::Linkly),
            other => Err(ParseUpstreamError::Unknown(other.to_owned())),
        }
    }
}

impl fmt::Display for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let based = |f: &mut fmt::Formatter<'_>, prefix: &str, base: &Url| {
            write!(f, "{prefix}+{}", base.as_str().trim_end_matches('/'))
        };
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
            Self::XGd => f.write_str("x.gd"),
            Self::Cuttly => f.write_str("cutt.ly"),
            Self::ShortIo => f.write_str("short.io"),
            Self::Dub => f.write_str("dub"),
            Self::Rebrandly => f.write_str("rebrandly"),
            Self::Bitly => f.write_str("bitly"),
            Self::Tly => f.write_str("t.ly"),
            Self::Linkly => f.write_str("linkly"),
            Self::Yourls(base) => based(f, "yourls", base),
            Self::Kutt(base) => based(f, "kutt", base),
            Self::Shlink(base) => based(f, "shlink", base),
            Self::Rail(base) => based(f, "rail", base),
        }
    }
}

impl Serialize for Upstream {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Secrets for keyed upstreams, by environment variable name. Never part of an upstream's name,
/// so `/api/topology` and logs can list upstreams freely.
#[derive(Clone, Default)]
pub struct Credentials(BTreeMap<&'static str, String>);

impl Credentials {
    /// Every secret RAIL knows how to use.
    pub const VARS: [&'static str; 14] = [
        "RAIL_TINYURL_TOKEN",
        "RAIL_YOURLS_SIGNATURE",
        "RAIL_XGD_KEY",
        "RAIL_CUTTLY_KEY",
        "RAIL_SHORTIO_KEY",
        "RAIL_SHORTIO_DOMAIN",
        "RAIL_DUB_TOKEN",
        "RAIL_REBRANDLY_KEY",
        "RAIL_BITLY_TOKEN",
        "RAIL_TLY_TOKEN",
        "RAIL_LINKLY_KEY",
        "RAIL_LINKLY_WORKSPACE",
        "RAIL_KUTT_KEY",
        "RAIL_SHLINK_KEY",
    ];

    /// Collect every known secret from `lookup` (normally the process environment). Empty
    /// values count as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        Self::VARS
            .into_iter()
            .fold(Self::default(), |creds, var| creds.with(var, lookup(var)))
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|var| std::env::var(var).ok())
    }

    /// Set (or, with `None` or an empty string, leave alone) one secret.
    #[must_use]
    pub fn with(mut self, var: &'static str, value: Option<impl Into<String>>) -> Self {
        if let Some(value) = value.map(Into::into).filter(|v| !v.is_empty()) {
            self.0.insert(var, value);
        }
        self
    }

    pub fn get(&self, var: &str) -> Option<&str> {
        self.0.get(var).map(String::as_str)
    }

    fn require(&self, var: &str, upstream: &Upstream) -> Result<&str> {
        self.get(var)
            .with_context(|| format!("upstream {upstream} needs {var}"))
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
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

#[derive(Debug, Serialize)]
pub struct ShortenRequest<'a> {
    pub url: &'a str,
}

#[derive(Debug, Deserialize)]
struct RailShortenResponse {
    short_url: String,
}

/// Where the hosted vendors' APIs live. Tests point these at a mock.
#[derive(Debug, Clone)]
struct Endpoints {
    tinyurl: Url,
    xgd: Url,
    cuttly: Url,
    shortio: Url,
    dub: Url,
    rebrandly: Url,
    bitly: Url,
    tly: Url,
    linkly: Url,
}

impl Endpoints {
    fn hosted() -> Self {
        let url = |s: &str| Url::parse(s).expect("static vendor URL");
        Self {
            tinyurl: url("https://api.tinyurl.com/create"),
            xgd: url("https://xgd.io/V1/shorten"),
            cuttly: url("https://cutt.ly/api/api.php"),
            shortio: url("https://api.short.io/links"),
            dub: url("https://api.dub.co/links"),
            rebrandly: url("https://api.rebrandly.com/v1/links"),
            bitly: url("https://api-ssl.bitly.com/v4/shorten"),
            tly: url("https://api.t.ly/api/v1/link/shorten"),
            linkly: url("https://app.linklyhq.com/api/v1/link"),
        }
    }
}

impl Upstream {
    /// The secrets (environment variable names) this upstream cannot work without.
    pub fn secrets(&self) -> &'static [&'static str] {
        match self {
            Self::TinyUrlApi => &["RAIL_TINYURL_TOKEN"],
            Self::Yourls(_) => &["RAIL_YOURLS_SIGNATURE"],
            Self::XGd => &["RAIL_XGD_KEY"],
            Self::Cuttly => &["RAIL_CUTTLY_KEY"],
            Self::ShortIo => &["RAIL_SHORTIO_KEY", "RAIL_SHORTIO_DOMAIN"],
            Self::Dub => &["RAIL_DUB_TOKEN"],
            Self::Rebrandly => &["RAIL_REBRANDLY_KEY"],
            Self::Bitly => &["RAIL_BITLY_TOKEN"],
            Self::Tly => &["RAIL_TLY_TOKEN"],
            Self::Linkly => &["RAIL_LINKLY_KEY", "RAIL_LINKLY_WORKSPACE"],
            Self::Kutt(_) => &["RAIL_KUTT_KEY"],
            Self::Shlink(_) => &["RAIL_SHLINK_KEY"],
            Self::IsGd
            | Self::VGd
            | Self::TinyUrl
            | Self::DaGd
            | Self::Ulvis
            | Self::SpooMe
            | Self::RluRu
            | Self::ClckRu
            | Self::RedirectTo(_)
            | Self::Rail(_) => &[],
        }
    }

    /// Ask this upstream to shorten `target`, returning the short URL it minted.
    pub async fn shorten(&self, ctx: &ShortenContext, target: &str) -> Result<String> {
        self.shorten_via(ctx, &Endpoints::hosted(), target).await
    }

    async fn shorten_via(
        &self,
        ctx: &ShortenContext,
        api: &Endpoints,
        target: &str,
    ) -> Result<String> {
        let client = &ctx.client;
        let key = |var: &str| ctx.credentials.require(var, self);
        let vendor = self.to_string();
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
                tinyurl_api(client, &api.tinyurl, key("RAIL_TINYURL_TOKEN")?, target).await?
            }
            Self::XGd => {
                let url = with_query(&api.xgd, [("url", target), ("key", key("RAIL_XGD_KEY")?)]);
                let body = api_json(&vendor, client.get(url)).await?;
                // Errors arrive as HTTP 200 with the real status inside.
                ensure!(
                    body["status"] == 200,
                    "{vendor} refused: {}",
                    message(&body)
                );
                pick(&vendor, &body, "/shorturl")?
            }
            Self::Cuttly => {
                let url = with_query(
                    &api.cuttly,
                    [("key", key("RAIL_CUTTLY_KEY")?), ("short", target)],
                );
                let body = api_json(&vendor, client.get(url)).await?;
                match body["url"]["status"].as_i64() {
                    Some(7) => pick(&vendor, &body, "/url/shortLink")?,
                    status => bail!("{vendor} refused: {}", cuttly_status(status)),
                }
            }
            Self::ShortIo => {
                let request = client
                    .post(api.shortio.clone())
                    // Short.io's secret key goes in bare, without a "Bearer" scheme.
                    .header("authorization", key("RAIL_SHORTIO_KEY")?)
                    .json(&json!({
                        "originalURL": target,
                        "domain": key("RAIL_SHORTIO_DOMAIN")?,
                    }));
                let body = api_json(&vendor, request).await?;
                pick(&vendor, &body, "/secureShortURL")
                    .or_else(|_| pick(&vendor, &body, "/shortURL"))?
            }
            Self::Dub => {
                let request = client
                    .post(api.dub.clone())
                    .bearer_auth(key("RAIL_DUB_TOKEN")?)
                    .json(&json!({ "url": target }));
                pick(&vendor, &api_json(&vendor, request).await?, "/shortLink")?
            }
            Self::Rebrandly => {
                let request = client
                    .post(api.rebrandly.clone())
                    .header("apikey", key("RAIL_REBRANDLY_KEY")?)
                    .json(&json!({ "destination": target }));
                let short = pick(&vendor, &api_json(&vendor, request).await?, "/shortUrl")?;
                // Rebrandly answers "rebrand.ly/abc", without a scheme.
                if short.contains("://") {
                    short
                } else {
                    format!("https://{short}")
                }
            }
            Self::Bitly => {
                let request = client
                    .post(api.bitly.clone())
                    .bearer_auth(key("RAIL_BITLY_TOKEN")?)
                    .json(&json!({ "long_url": target }));
                pick(&vendor, &api_json(&vendor, request).await?, "/link")?
            }
            Self::Tly => {
                let request = client
                    .post(api.tly.clone())
                    .bearer_auth(key("RAIL_TLY_TOKEN")?)
                    .json(&json!({ "long_url": target }));
                pick(&vendor, &api_json(&vendor, request).await?, "/short_url")?
            }
            Self::Linkly => {
                // Linkly takes its key in the body, not a header.
                let request = client.post(api.linkly.clone()).json(&json!({
                    "api_key": key("RAIL_LINKLY_KEY")?,
                    "workspace_id": key("RAIL_LINKLY_WORKSPACE")?,
                    "url": target,
                }));
                pick(&vendor, &api_json(&vendor, request).await?, "/full_url")?
            }
            Self::Yourls(base) => {
                yourls(client, base, key("RAIL_YOURLS_SIGNATURE")?, target).await?
            }
            Self::Kutt(base) => {
                let request = client
                    .post(base.join("api/v2/links")?)
                    .header("x-api-key", key("RAIL_KUTT_KEY")?)
                    .json(&json!({ "target": target, "reuse": true }));
                pick(&vendor, &api_json(&vendor, request).await?, "/link")?
            }
            Self::Shlink(base) => {
                let request = client
                    .post(base.join("rest/v3/short-urls")?)
                    .header("x-api-key", key("RAIL_SHLINK_KEY")?)
                    // Without it, Shlink may describe errors in HTML.
                    .header("accept", "application/json")
                    .json(&json!({ "longUrl": target, "findIfExists": true }));
                pick(&vendor, &api_json(&vendor, request).await?, "/shortUrl")?
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
        for var in upstream.secrets() {
            credentials.require(var, &upstream)?;
        }
        upstreams.push(upstream);
    }
    Ok(upstreams)
}

fn with_query<'a>(endpoint: &Url, params: impl IntoIterator<Item = (&'a str, &'a str)>) -> Url {
    let mut url = endpoint.clone();
    url.query_pairs_mut().extend_pairs(params);
    url
}

/// Send a vendor API request and return its JSON body, turning HTTP failures into errors that
/// carry the vendor's own explanation.
async fn api_json(vendor: &str, request: RequestBuilder) -> Result<Value> {
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;
    let body: Option<Value> = serde_json::from_str(&text).ok();
    if !status.is_success() {
        let reason = body
            .as_ref()
            .map(message)
            .unwrap_or_else(|| snippet(&text).to_owned());
        bail!("{vendor} refused ({status}): {reason}");
    }
    body.with_context(|| {
        format!(
            "{vendor} answered with something that is not JSON: {:?}",
            snippet(&text)
        )
    })
}

/// The human-readable part of a vendor's error body, wherever that vendor keeps it.
fn message(body: &Value) -> String {
    for pointer in [
        "/message",
        "/error/message",
        "/error",
        "/detail",
        "/description",
        "/errors/0/message",
        "/errors/0",
    ] {
        if let Some(text) = body.pointer(pointer).and_then(Value::as_str) {
            return text.to_owned();
        }
    }
    snippet(&body.to_string()).to_owned()
}

fn snippet(text: &str) -> &str {
    let text = text.trim();
    match text.char_indices().nth(200) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

fn pick(vendor: &str, body: &Value, pointer: &str) -> Result<String> {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("{vendor} answered without {pointer}: {}", message(body)))
}

fn cuttly_status(status: Option<i64>) -> &'static str {
    match status {
        Some(1) => "the link is already shortened",
        Some(2) => "not a link",
        Some(3) => "alias taken",
        Some(4) => "invalid API key",
        Some(5) => "the link failed validation",
        Some(6) => "the link is from a blocked domain",
        Some(8) => "monthly link limit reached",
        _ => "unexpected status",
    }
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
        .json(&json!({ "long_url": target }))
        .send()
        .await?
        .error_for_status()?
        .json::<Response>()
        .await?
        .short_url)
}

async fn tinyurl_api(client: &Client, endpoint: &Url, token: &str, target: &str) -> Result<String> {
    // On failure `data` is an empty array rather than null, so it can't be typed up front.
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        data: Value,
        #[serde(default)]
        errors: Vec<String>,
    }
    let response: Response = client
        .post(endpoint.clone())
        .bearer_auth(token)
        .json(&json!({ "url": target }))
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
    let url = with_query(
        &base.join("yourls-api.php")?,
        [
            ("action", "shorturl"),
            ("format", "json"),
            ("signature", signature),
            ("url", target),
        ],
    );
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
    use axum::{
        Json, Router,
        extract::{Query, Request},
        http::{HeaderMap, StatusCode},
        routing::{any, get, post},
    };
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn round_trips_through_display() {
        for spec in KEYWORDS.into_iter().chain([
            "rail+http://leaf-a.rail.localhost:8081",
            "yourls+https://sho.rt/y",
            "kutt+https://kutt.example",
            "shlink+https://s.example/prefix",
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
            "kutt+javascript:alert(1)",
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
    fn keyed_upstreams_need_every_key() {
        let none = Credentials::default();
        for spec in [
            "tinyurl-api",
            "yourls+https://sho.rt",
            "x.gd",
            "kutt+https://k.example",
        ] {
            let err = parse_all([spec], &none).unwrap_err();
            assert!(err.to_string().contains("needs RAIL_"), "{spec}: {err}");
        }
        // Short.io needs its domain as well as its key.
        let half = Credentials::default().with("RAIL_SHORTIO_KEY", Some("k"));
        let err = parse_all(["short.io"], &half).unwrap_err();
        assert!(err.to_string().contains("RAIL_SHORTIO_DOMAIN"), "{err}");

        let all = Credentials::from_lookup(|_| Some("set".into()));
        let every: Vec<String> = KEYWORDS
            .iter()
            .map(|s| s.to_string())
            .chain(["yourls+https://y", "kutt+https://k", "shlink+https://s"].map(String::from))
            .collect();
        assert!(parse_all(every.iter().map(String::as_str), &all).is_ok());
    }

    #[test]
    fn every_secret_an_upstream_needs_is_known() {
        for spec in
            KEYWORDS
                .into_iter()
                .chain(["yourls+https://y", "kutt+https://k", "shlink+https://s"])
        {
            for var in spec.parse::<Upstream>().unwrap().secrets() {
                assert!(Credentials::VARS.contains(var), "{var} is not in VARS");
            }
        }
    }

    #[test]
    fn credentials_never_print_and_ignore_blanks() {
        let keys = Credentials::from_lookup(|var| match var {
            "RAIL_TINYURL_TOKEN" => Some("hunter2".into()),
            "RAIL_XGD_KEY" => Some(String::new()),
            _ => None,
        });
        let debug = format!("{keys:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("RAIL_TINYURL_TOKEN"), "{debug}");
        assert_eq!(keys.get("RAIL_XGD_KEY"), None);
    }

    #[test]
    fn rejects_unknown_shorteners() {
        assert!(matches!(
            "goo.gl".parse::<Upstream>(),
            Err(ParseUpstreamError::Unknown(_))
        ));
    }

    fn ctx(credentials: Credentials) -> ShortenContext {
        ShortenContext {
            client: Client::new(),
            chain: Arc::default(),
            credentials: Arc::new(credentials),
        }
    }

    #[tokio::test]
    async fn redirect_services_build_links_without_a_request() -> Result<()> {
        let link = Upstream::RedirectTo(RedirectService::PostmanEcho)
            .shorten(
                &ctx(Credentials::default()),
                "https://example.com/a b?c=d&e",
            )
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

    /// A fake of every hosted vendor's API: each accepts only the secret "good" in the place
    /// that vendor expects it, and answers in that vendor's shape.
    async fn vendors() -> Result<Endpoints> {
        async fn handle(request: Request) -> (StatusCode, Json<Value>) {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(body, 1 << 16)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let query: HashMap<String, String> = parts
                .uri
                .query()
                .map(|q| {
                    url::form_urlencoded::parse(q.as_bytes())
                        .into_owned()
                        .collect()
                })
                .unwrap_or_default();
            let header = |name: &str| {
                parts
                    .headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned()
            };
            let bearer = header("authorization") == "Bearer good";
            let denied = (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "message": "bad credentials" })),
            );
            let ok = |v: Value| (StatusCode::OK, Json(v));
            match parts.uri.path() {
                "/xgd" if query.get("key").is_some_and(|k| k == "good") => ok(json!({
                    "status": 200, "shorturl": "https://x.gd/abc"
                })),
                "/xgd" => ok(json!({ "status": 401, "message": "Invalid API key." })),
                "/cuttly" if query.get("key").is_some_and(|k| k == "good") => ok(json!({
                    "url": { "status": 7, "shortLink": "https://cutt.ly/abc" }
                })),
                "/cuttly" => ok(json!({ "url": { "status": 4 } })),
                "/shortio" if header("authorization") == "good" => ok(json!({
                    "secureShortURL": format!("https://{}/abc", body["domain"].as_str().unwrap_or("?"))
                })),
                "/dub" if bearer => ok(json!({ "shortLink": "https://dub.sh/abc" })),
                "/rebrandly" if header("apikey") == "good" => {
                    ok(json!({ "shortUrl": "rebrand.ly/abc" }))
                }
                "/bitly" if bearer => ok(json!({ "link": "https://bit.ly/abc" })),
                "/tly" if bearer => ok(json!({ "short_url": "https://t.ly/abc" })),
                "/linkly" if body["api_key"] == "good" && body["workspace_id"] == "good" => {
                    ok(json!({ "full_url": "https://l.linklyhq.com/abc" }))
                }
                "/api/v2/links" if header("x-api-key") == "good" && body["reuse"] == true => {
                    ok(json!({ "link": "https://kutt.example/abc" }))
                }
                "/rest/v3/short-urls"
                    if header("x-api-key") == "good" && body["findIfExists"] == true =>
                {
                    ok(json!({ "shortUrl": "https://s.example/abc" }))
                }
                _ => denied,
            }
        }
        let base = mock(Router::new().fallback(any(handle))).await?;
        let at = |path: &str| base.join(path).expect("joins");
        Ok(Endpoints {
            tinyurl: at("tinyurl"),
            xgd: at("xgd"),
            cuttly: at("cuttly"),
            shortio: at("shortio"),
            dub: at("dub"),
            rebrandly: at("rebrandly"),
            bitly: at("bitly"),
            tly: at("tly"),
            linkly: at("linkly"),
        })
    }

    #[tokio::test]
    async fn keyed_vendors_send_their_secrets_and_read_their_answers() -> Result<()> {
        let api = vendors().await?;
        let self_hosted = Url::parse(api.xgd.origin().ascii_serialization().as_str())?;
        let good = Credentials::from_lookup(|var| {
            Some(if var == "RAIL_SHORTIO_DOMAIN" {
                "rail.short.gy".into()
            } else {
                "good".into()
            })
        });
        let bad = Credentials::from_lookup(|_| Some("bad".into()));
        let cases = [
            (Upstream::XGd, "https://x.gd/abc"),
            (Upstream::Cuttly, "https://cutt.ly/abc"),
            (Upstream::ShortIo, "https://rail.short.gy/abc"),
            (Upstream::Dub, "https://dub.sh/abc"),
            (Upstream::Rebrandly, "https://rebrand.ly/abc"),
            (Upstream::Bitly, "https://bit.ly/abc"),
            (Upstream::Tly, "https://t.ly/abc"),
            (Upstream::Linkly, "https://l.linklyhq.com/abc"),
            (
                Upstream::Kutt(self_hosted.clone()),
                "https://kutt.example/abc",
            ),
            (Upstream::Shlink(self_hosted), "https://s.example/abc"),
        ];
        for (upstream, expected) in cases {
            let minted = upstream
                .shorten_via(&ctx(good.clone()), &api, "https://example.com/")
                .await?;
            assert_eq!(minted, expected, "{upstream}");
            let err = upstream
                .shorten_via(&ctx(bad.clone()), &api, "https://example.com/")
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&upstream.to_string()) && err.contains("refused"),
                "{upstream}: {err}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn vendor_errors_carry_the_vendors_reason() -> Result<()> {
        let api = vendors().await?;
        let bad = Credentials::from_lookup(|_| Some("bad".into()));
        let err = Upstream::XGd
            .shorten_via(&ctx(bad.clone()), &api, "https://example.com/")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid API key."), "{err}");
        let err = Upstream::Cuttly
            .shorten_via(&ctx(bad.clone()), &api, "https://example.com/")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid API key"), "{err}");
        let err = Upstream::Dub
            .shorten_via(&ctx(bad), &api, "https://example.com/")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bad credentials"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn tinyurl_api_sends_the_token_and_reads_tiny_url() -> Result<()> {
        let app = Router::new().route(
            "/create",
            post(async |headers: HeaderMap, Json(body): Json<Value>| {
                if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer t0k") {
                    return Json(json!({ "data": [], "code": 1, "errors": ["Unauthenticated."] }));
                }
                Json(json!({
                    "data": { "tiny_url": format!("https://tinyurl.com/for-{}", body["url"].as_str().unwrap().len()) },
                    "code": 0,
                    "errors": []
                }))
            }),
        );
        let endpoint = mock(app).await?.join("create")?;
        let client = Client::new();
        let ok = tinyurl_api(&client, &endpoint, "t0k", "https://example.com").await?;
        assert_eq!(ok, "https://tinyurl.com/for-19");
        let err = tinyurl_api(&client, &endpoint, "wrong", "https://example.com")
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
                    ("sig", "https://example.com/new") => json!({
                        "status": "success", "shorturl": "https://sho.rt/new"
                    }),
                    // YOURLS reports a duplicate as a failure that still carries the link.
                    ("sig", _) => json!({
                        "status": "fail", "code": "error:url", "shorturl": "https://sho.rt/old"
                    }),
                    _ => json!({ "message": "Please log in", "errorCode": "403" }),
                };
                (StatusCode::OK, Json(body))
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
