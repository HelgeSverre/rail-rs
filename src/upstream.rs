//! The inexpensive links in the Redundant Array of Inexpensive Links.

use std::{fmt, str::FromStr};

use anyhow::{Context, Result, ensure};
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
    /// Another RAIL instance. Load balancers all the way down.
    Rail(Url),
}

#[derive(Debug, thiserror::Error)]
pub enum ParseUpstreamError {
    #[error("unknown upstream {0:?} (expected is.gd, v.gd, tinyurl, da.gd, ulvis or rail+<url>)")]
    Unknown(String),
    #[error("invalid rail upstream URL")]
    BadRailUrl(#[from] url::ParseError),
    #[error("rail upstream must be an http(s) base URL, got {0}")]
    NotHttp(Url),
}

impl FromStr for Upstream {
    type Err = ParseUpstreamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(base) = s.strip_prefix("rail+") {
            let base = Url::parse(base)?;
            if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() {
                return Err(ParseUpstreamError::NotHttp(base));
            }
            return Ok(Self::Rail(base));
        }
        match s {
            "is.gd" => Ok(Self::IsGd),
            "v.gd" => Ok(Self::VGd),
            "tinyurl" => Ok(Self::TinyUrl),
            "da.gd" => Ok(Self::DaGd),
            "ulvis" => Ok(Self::Ulvis),
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
            Self::Rail(base) => write!(f, "rail+{}", base.as_str().trim_end_matches('/')),
        }
    }
}

impl Serialize for Upstream {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
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

impl Upstream {
    /// Ask this upstream to shorten `target`, returning the short URL it minted. `chain` is the
    /// list of RAIL instance ids this request has already passed through, forwarded to nested
    /// RAILs so a loop terminates even when cycle detection misses it.
    pub async fn shorten(&self, client: &Client, target: &str, chain: &[String]) -> Result<String> {
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
            Self::Rail(base) => {
                client
                    .post(base.join("api/shorten")?)
                    .header(X_RAIL_CHAIN, chain.join(","))
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
/// because every mirror is keyed by its upstream.
pub fn parse_all<'a>(specs: impl IntoIterator<Item = &'a str>) -> Result<Vec<Upstream>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_display() {
        for spec in [
            "is.gd",
            "v.gd",
            "tinyurl",
            "da.gd",
            "ulvis",
            "rail+http://leaf-a.rail.localhost:8081",
        ] {
            let upstream: Upstream = spec.parse().unwrap();
            assert_eq!(upstream.to_string(), spec);
            assert_eq!(upstream.to_string().parse::<Upstream>().unwrap(), upstream);
        }
    }

    #[test]
    fn rejects_non_http_rail_upstreams() {
        for spec in ["rail+mailto:x", "rail+rail+http://x", "rail+ftp://x"] {
            assert!(
                spec.parse::<Upstream>().is_err(),
                "{spec} should be rejected"
            );
        }
    }

    #[test]
    fn parse_all_rejects_duplicates_and_skips_none() {
        assert_eq!(parse_all(["none", ""]).unwrap(), vec![]);
        assert_eq!(parse_all(["is.gd", "v.gd"]).unwrap().len(), 2);
        assert!(parse_all(["is.gd", "is.gd"]).is_err());
        // The same instance written two ways is still the same instance.
        assert!(parse_all(["rail+http://a:1", "rail+http://a:1/"]).is_err());
    }

    #[test]
    fn rejects_unknown_shorteners() {
        assert!(matches!(
            "bit.ly".parse::<Upstream>(),
            Err(ParseUpstreamError::Unknown(_))
        ));
    }
}
