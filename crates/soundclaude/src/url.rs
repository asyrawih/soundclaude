//! URL classification and normalization for the shapes `SoundCloud` hands out.

use crate::error::{Error, Result};
use crate::http;
use regex::Regex;
use std::sync::LazyLock;
use url::Url;

/// Prefix of the "personalized tracks" pseudo-set the discover feed links to.
const PERSONALIZED_PREFIX: &str = "https://soundcloud.com/discover/sets/personalized-tracks::";

/// Any absolute http(s) url, used to sift real links out of the firebase HTML.
static ANY_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://(?:www\.)?[-a-zA-Z0-9@:%._+~#=]{1,256}\.[a-zA-Z0-9()]{1,63}\b[-a-zA-Z0-9()@:%_+.~#?&/\\=]*")
        .expect("ANY_URL regex")
});

/// `\uXXXX` escapes that show up inside the firebase page's inlined JSON.
static UNICODE_ESCAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\u([0-9a-fA-F]{4})").expect("UNICODE_ESCAPE regex"));

fn host_of(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

/// True for `soundcloud.com` / `m.soundcloud.com` links that actually name a resource,
/// and for the mobile-app firebase shortlinks.
pub fn is_valid_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };

    match host.as_str() {
        // A bare `soundcloud.com/` with no path names nothing.
        "soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com" | "on.soundcloud.com" => {
            !parsed.path().trim_matches('/').is_empty()
        }
        "soundcloud.app.goo.gl" => !parsed.path().trim_matches('/').is_empty(),
        _ => false,
    }
}

/// True when the url points at a set (`/sets/<slug>`) rather than a single track.
pub fn is_playlist_url(url: &str) -> bool {
    if !is_valid_url(url) {
        return false;
    }
    Url::parse(url).is_ok_and(|u| u.path().contains("/sets/"))
}

/// True for `.../discover/sets/personalized-tracks::user-xxxx:123456789`.
pub fn is_personalized_track_url(url: &str) -> bool {
    url.starts_with(PERSONALIZED_PREFIX)
}

/// Pull the numeric track id out of a personalized-tracks url.
pub fn extract_personalized_track_id(url: &str) -> Option<u64> {
    if !is_personalized_track_url(url) {
        return None;
    }
    // https:, //soundcloud.com/..., <owner>, <id>  -> the id is the last colon-separated part
    url.rsplit(':').next()?.trim().parse().ok()
}

/// Rewrite `m.soundcloud.com` to `soundcloud.com`; other urls pass through untouched.
pub fn strip_mobile_prefix(url: &str) -> String {
    match host_of(url).as_deref() {
        Some("m.soundcloud.com") => {
            let Ok(mut parsed) = Url::parse(url) else {
                return url.to_string();
            };
            if parsed.set_host(Some("soundcloud.com")).is_err() {
                return url.to_string();
            }
            parsed.to_string()
        }
        _ => url.to_string(),
    }
}

/// True for the `https://soundcloud.app.goo.gl/xxxx` links the mobile app shares.
pub fn is_firebase_url(url: &str) -> bool {
    matches!(host_of(url).as_deref(), Some("soundcloud.app.goo.gl"))
}

/// Follow a firebase shortlink to the canonical `soundcloud.com` url.
///
/// Appending `d=1` makes the dynamic-link endpoint render a debug page whose HTML
/// embeds the destination, so no app-store redirect chain has to be followed.
pub async fn resolve_firebase_url(http_client: &reqwest::Client, url: &str) -> Result<String> {
    let mut probe = Url::parse(url)?;
    probe.query_pairs_mut().append_pair("d", "1");

    let res = http_client.get(probe.as_str()).send().await?;
    let body = http::check(res).await?.text().await?;

    let found = ANY_URL
        .find_iter(&body)
        .map(|m| unescape_unicode(m.as_str()))
        .find(|candidate| {
            matches!(
                host_of(candidate).as_deref(),
                Some("soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com")
            )
        })
        .ok_or_else(|| Error::FirebaseUnresolved(url.to_string()))?;

    Ok(strip_mobile_prefix(&found))
}

/// Replace `\uXXXX` escapes with the characters they denote.
fn unescape_unicode(s: &str) -> String {
    UNICODE_ESCAPE
        .replace_all(s, |caps: &regex::Captures<'_>| {
            u32::from_str_radix(&caps[1], 16)
                .ok()
                .and_then(char::from_u32)
                .map_or_else(|| caps[0].to_string(), |c| c.to_string())
        })
        .into_owned()
}

/// Append `key=value` pairs to a url, preserving whatever query it already had.
pub(crate) fn append_query(url: &str, params: &[(&str, &str)]) -> Result<String> {
    let mut parsed = Url::parse(url)?;
    {
        let mut q = parsed.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    Ok(parsed.into())
}

/// Overwrite a single query parameter, adding it when absent.
///
/// Used to rewrite the `limit` on a `next_href` so the final page of a paginated
/// walk doesn't drag down entries that would only be discarded.
pub(crate) fn set_query(url: &str, key: &str, value: &str) -> Result<String> {
    let parsed = Url::parse(url)?;
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| k != key)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    let mut out = parsed.clone();
    {
        let mut q = out.query_pairs_mut();
        q.clear();
        for (k, v) in &kept {
            q.append_pair(k, v);
        }
        q.append_pair(key, value);
    }
    Ok(out.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_track_urls() {
        assert!(is_valid_url("https://soundcloud.com/artist/track"));
        assert!(is_valid_url("http://soundcloud.com/artist/sets/album"));
        assert!(is_valid_url("https://m.soundcloud.com/artist/track"));
        assert!(is_valid_url("https://soundcloud.app.goo.gl/abcd"));
    }

    #[test]
    fn rejects_non_tracks() {
        assert!(!is_valid_url("https://soundcloud.com/"));
        assert!(!is_valid_url("https://example.com/artist/track"));
        assert!(!is_valid_url("not a url"));
        assert!(!is_valid_url("ftp://soundcloud.com/artist/track"));
    }

    #[test]
    fn detects_playlists() {
        assert!(is_playlist_url("https://soundcloud.com/artist/sets/album"));
        assert!(!is_playlist_url("https://soundcloud.com/artist/track"));
        assert!(!is_playlist_url("https://example.com/a/sets/b"));
    }

    #[test]
    fn strips_the_mobile_host_only() {
        assert_eq!(
            strip_mobile_prefix("https://m.soundcloud.com/artist/track"),
            "https://soundcloud.com/artist/track"
        );
        assert_eq!(
            strip_mobile_prefix("https://soundcloud.com/artist/track"),
            "https://soundcloud.com/artist/track"
        );
    }

    #[test]
    fn parses_personalized_track_ids() {
        let url = "https://soundcloud.com/discover/sets/personalized-tracks::user-123456:987654321";
        assert!(is_personalized_track_url(url));
        assert_eq!(extract_personalized_track_id(url), Some(987_654_321));
        assert_eq!(
            extract_personalized_track_id("https://soundcloud.com/artist/track"),
            None
        );
    }

    #[test]
    fn query_helpers_preserve_existing_params() {
        let out = append_query("https://api.example.com/x?a=1", &[("client_id", "abc")]).unwrap();
        assert!(out.contains("a=1") && out.contains("client_id=abc"));

        let out = set_query("https://api.example.com/x?limit=200&a=1", "limit", "5").unwrap();
        assert!(out.contains("limit=5") && out.contains("a=1"));
        assert!(!out.contains("limit=200"));

        // Absent keys are added rather than ignored.
        let out = set_query("https://api.example.com/x", "limit", "5").unwrap();
        assert!(out.contains("limit=5"));
    }

    #[test]
    fn unescapes_unicode_sequences() {
        assert_eq!(
            unescape_unicode(r"https://soundcloud.com/a\u003fb"),
            "https://soundcloud.com/a?b"
        );
        assert_eq!(unescape_unicode("nothing to do"), "nothing to do");
    }
}
