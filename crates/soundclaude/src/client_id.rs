//! Scraping a usable `client_id` out of the public soundcloud.com bundles.
//!
//! `SoundCloud` has no public key issuance any more; the web player ships its own id
//! inside one of the `a-v2.sndcdn.com/assets/*.js` chunks. This mirrors what the
//! `soundcloud-key-fetch` package did, plus an on-disk cache with a TTL.

use crate::error::{Error, Result};
use crate::http;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

const HOMEPAGE: &str = "https://soundcloud.com/";

/// Cached ids older than this are re-scraped; `SoundCloud` rotates roughly daily.
pub const DEFAULT_TTL_SECS: u64 = 60 * 60 * 24;

static SCRIPT_SRC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<script[^>]+src="([^"]+)""#).expect("SCRIPT_SRC regex"));

/// `client_id:"xxxx"` as it appears minified inside the bundles.
static CLIENT_ID_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"client_id\s*[:=]\s*["']([a-zA-Z0-9]{32})["']"#).expect("CLIENT_ID_LITERAL regex")
});

/// `client_id=xxxx` as it appears in urls baked into the bundles.
static CLIENT_ID_QUERY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"client_id=([a-zA-Z0-9]{32})").expect("CLIENT_ID_QUERY regex"));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedClientId {
    pub client_id: String,
    /// Unix seconds at which the id was scraped.
    pub fetched_at: u64,
}

impl CachedClientId {
    pub fn new(client_id: String) -> Self {
        Self {
            client_id,
            fetched_at: now_secs(),
        }
    }

    pub fn age_secs(&self) -> u64 {
        now_secs().saturating_sub(self.fetched_at)
    }

    pub fn is_fresh(&self, ttl_secs: u64) -> bool {
        !self.client_id.is_empty() && self.age_secs() < ttl_secs
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Scrape soundcloud.com for a working `client_id`.
///
/// The id lives in one of the asset chunks; the later chunks hold it far more
/// often than the early ones, so they are tried in reverse document order.
pub async fn fetch(client: &reqwest::Client) -> Result<String> {
    let res = client.get(HOMEPAGE).send().await?;
    let html = http::check(res).await?.text().await?;

    // The homepage itself occasionally inlines the id.
    if let Some(id) = scan(&html) {
        tracing::debug!("client_id found inline on the homepage");
        return Ok(id);
    }

    let scripts: Vec<String> = SCRIPT_SRC
        .captures_iter(&html)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .filter(|src| src.starts_with("http"))
        .collect();

    if scripts.is_empty() {
        return Err(Error::ClientIdNotFound);
    }

    for src in scripts.iter().rev() {
        let Ok(res) = client.get(src).send().await else {
            continue;
        };
        if !res.status().is_success() {
            continue;
        }
        let Ok(body) = res.text().await else { continue };
        if let Some(id) = scan(&body) {
            tracing::debug!(bundle = %src, "scraped client_id");
            return Ok(id);
        }
    }

    Err(Error::ClientIdNotFound)
}

fn scan(body: &str) -> Option<String> {
    CLIENT_ID_LITERAL
        .captures(body)
        .or_else(|| CLIENT_ID_QUERY.captures(body))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Read a cached id, returning `None` when missing, unreadable, or stale.
pub async fn read_cache(path: &Path, ttl_secs: u64) -> Option<CachedClientId> {
    let raw = tokio::fs::read_to_string(path).await.ok()?;
    let cached: CachedClientId = serde_json::from_str(&raw).ok()?;
    cached.is_fresh(ttl_secs).then_some(cached)
}

/// Persist an id next to whatever path the caller chose, creating parent dirs.
pub async fn write_cache(path: &Path, cached: &CachedClientId) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    tokio::fs::write(path, serde_json::to_vec(cached)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ids_in_both_shapes() {
        let literal = r#"var o={client_id:"aBcDeFgHiJkLmNoPqRsTuVwXyZ012345",x:1}"#;
        assert_eq!(
            scan(literal).as_deref(),
            Some("aBcDeFgHiJkLmNoPqRsTuVwXyZ012345")
        );

        let query = "https://api-v2.soundcloud.com/me?client_id=0123456789abcdefghijklmnopqrstuv";
        assert_eq!(
            scan(query).as_deref(),
            Some("0123456789abcdefghijklmnopqrstuv")
        );

        assert_eq!(scan("client_id:\"tooshort\""), None);
    }

    #[test]
    fn extracts_script_sources() {
        let html = r#"<script crossorigin src="https://a-v2.sndcdn.com/assets/0-abc.js"></script>
                      <script src="/inline.js"></script>"#;
        let found: Vec<_> = SCRIPT_SRC
            .captures_iter(html)
            .map(|c| c[1].to_string())
            .collect();
        assert_eq!(found.len(), 2);
        assert!(found[0].starts_with("https://a-v2.sndcdn.com"));
    }

    #[test]
    fn cache_freshness_follows_ttl() {
        let mut c = CachedClientId::new("x".repeat(32));
        assert!(c.is_fresh(DEFAULT_TTL_SECS));
        c.fetched_at = now_secs().saturating_sub(DEFAULT_TTL_SECS + 1);
        assert!(!c.is_fresh(DEFAULT_TTL_SECS));
    }
}
