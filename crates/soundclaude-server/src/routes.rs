//! HTTP surface: metadata as JSON, audio as a streaming body.

use crate::error::{ApiError, ApiResult};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use soundclaude::{Client, DownloadOptions, Format, PaginatedQuery, Protocol};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub scdl: Arc<Client>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/resolve", get(resolve))
        .route("/v1/track", get(track))
        .route("/v1/set", get(set))
        .route("/v1/user", get(user))
        .route("/v1/likes", get(likes))
        .route("/v1/search", get(search))
        .route("/v1/related", get(related))
        .route("/v1/stream", get(stream))
        .route("/v1/download", get(download))
        .fallback(not_found)
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "no_such_route", "no such route")
}

#[derive(Debug, Deserialize)]
pub struct UrlQuery {
    pub url: String,
}

async fn resolve(State(state): State<AppState>, Query(q): Query<UrlQuery>) -> ApiResult<Response> {
    let resolved = state.scdl.resolve(&q.url).await?;
    Ok(Json(resolved).into_response())
}

async fn track(State(state): State<AppState>, Query(q): Query<UrlQuery>) -> ApiResult<Response> {
    let track = state.scdl.track(&q.url).await?;
    Ok(Json(track).into_response())
}

async fn set(State(state): State<AppState>, Query(q): Query<UrlQuery>) -> ApiResult<Response> {
    let set = state.scdl.set(&q.url).await?;
    Ok(Json(set).into_response())
}

async fn user(State(state): State<AppState>, Query(q): Query<UrlQuery>) -> ApiResult<Response> {
    let user = state.scdl.user(&q.url).await?;
    Ok(Json(user).into_response())
}

#[derive(Debug, Deserialize)]
pub struct LikesQuery {
    /// A profile url. Mutually exclusive with `id`.
    pub url: Option<String>,
    /// A numeric user id. Mutually exclusive with `url`.
    pub id: Option<u64>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
    /// Include liked playlists alongside liked tracks.
    #[serde(default)]
    pub playlists: bool,
}

async fn likes(State(state): State<AppState>, Query(q): Query<LikesQuery>) -> ApiResult<Response> {
    let user_id = match (q.id, q.url.as_deref()) {
        (Some(id), _) => id,
        (None, Some(url)) => state.scdl.user(url).await?.id,
        (None, None) => return Err(ApiError::bad_request("one of `id` or `url` is required")),
    };

    // Unbounded paging is a request one caller can turn into hundreds of upstream
    // hits, so the server always caps it — unlike the library, which allows `None`.
    let limit = Some(q.limit.clamp(1, 500));

    let likes = if q.playlists {
        state.scdl.likes_raw(user_id, limit, q.offset).await?
    } else {
        state.scdl.likes(user_id, limit, q.offset).await?
    };

    Ok(Json(likes).into_response())
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

fn default_kind() -> String {
    "tracks".to_string()
}

fn default_limit() -> u32 {
    20
}

const VALID_KINDS: [&str; 5] = ["tracks", "users", "albums", "playlists", "all"];

async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Response> {
    if q.q.trim().is_empty() {
        return Err(ApiError::bad_request("`q` must not be empty"));
    }
    if !VALID_KINDS.contains(&q.kind.as_str()) {
        return Err(ApiError::bad_request(format!(
            "`kind` must be one of {}",
            VALID_KINDS.join(", ")
        )));
    }

    // Cap the page size so one caller cannot make us hammer the upstream api.
    let limit = q.limit.clamp(1, 200);
    let page: PaginatedQuery<Value> = state.scdl.search(&q.q, &q.kind, limit, q.offset).await?;
    Ok(Json(page).into_response())
}

#[derive(Debug, Deserialize)]
pub struct RelatedQuery {
    pub id: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

async fn related(
    State(state): State<AppState>,
    Query(q): Query<RelatedQuery>,
) -> ApiResult<Response> {
    let page = state
        .scdl
        .related(q.id, q.limit.clamp(1, 200), q.offset)
        .await?;
    Ok(Json(page).into_response())
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    pub url: String,
    pub format: Option<String>,
    pub protocol: Option<String>,
    /// Skip the artist's original-file download even when it is offered.
    #[serde(default)]
    pub no_direct: bool,
}

impl StreamQuery {
    fn to_options(&self) -> ApiResult<DownloadOptions> {
        let mut opts = DownloadOptions::new().use_download_link(!self.no_direct);

        if let Some(f) = &self.format {
            let format: Format = f.parse().map_err(ApiError::bad_request)?;
            opts = opts.format(format);
        }
        if let Some(p) = &self.protocol {
            let protocol: Protocol = p.parse().expect("infallible");
            if let Protocol::Other(other) = &protocol {
                return Err(ApiError::bad_request(format!(
                    "unknown protocol `{other}` (expected `progressive` or `hls`)"
                )));
            }
            opts = opts.protocol(protocol);
        }
        Ok(opts)
    }
}

async fn stream(
    State(state): State<AppState>,
    Query(q): Query<StreamQuery>,
) -> ApiResult<Response> {
    audio_response(state, q, Disposition::Inline).await
}

async fn download(
    State(state): State<AppState>,
    Query(q): Query<StreamQuery>,
) -> ApiResult<Response> {
    audio_response(state, q, Disposition::Attachment).await
}

#[derive(Clone, Copy)]
enum Disposition {
    Inline,
    Attachment,
}

impl Disposition {
    fn as_str(self) -> &'static str {
        match self {
            Disposition::Inline => "inline",
            Disposition::Attachment => "attachment",
        }
    }
}

async fn audio_response(
    state: AppState,
    q: StreamQuery,
    disposition: Disposition,
) -> ApiResult<Response> {
    let opts = q.to_options()?;
    let track = state.scdl.track(&q.url).await?;
    let audio = state.scdl.download_track(&track, &opts).await?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&audio.mime_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if let Some(len) = audio.content_length {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(disposition, &audio.filename),
    );
    // Byte ranges would have to be served by re-requesting upstream; be explicit.
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=0, no-store"),
    );

    tracing::info!(
        track = track.id,
        protocol = %audio.protocol,
        mime = %audio.mime_type,
        "streaming audio"
    );

    Ok((headers, Body::from_stream(audio.into_stream())).into_response())
}

/// Build a `Content-Disposition` value that survives non-ASCII titles.
///
/// The bare `filename=` is ASCII-folded for old clients; `filename*=` carries the
/// real name per RFC 5987.
fn content_disposition(kind: Disposition, filename: &str) -> HeaderValue {
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();

    let encoded = percent_encode(filename);
    let value = format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{}",
        kind.as_str(),
        ascii,
        encoded
    );

    HeaderValue::from_str(&value)
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"audio\""))
}

/// Percent-encode everything outside the RFC 5987 `attr-char` set.
fn percent_encode(s: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric()
            || matches!(
                c,
                '!' | '#' | '$' | '&' | '+' | '-' | '.' | '^' | '_' | '`' | '|' | '~'
            )
        {
            out.push(c);
        } else {
            // write! into the buffer rather than allocating a String per byte.
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_handles_unicode_titles() {
        let value = content_disposition(Disposition::Attachment, "Björk - Jóga.mp3");
        let value = value.to_str().unwrap();
        assert!(value.starts_with("attachment; "));
        // The ASCII fallback must not contain raw non-ASCII bytes...
        assert!(value.is_ascii());
        // ...and the encoded form must round-trip the real name.
        assert!(value.contains("filename*=UTF-8''"));
        assert!(value.contains("Bj%C3%B6rk"));
    }

    #[test]
    fn disposition_escapes_quotes() {
        let value = content_disposition(Disposition::Inline, "a\"b\\c.mp3");
        let value = value.to_str().unwrap();
        assert!(value.starts_with("inline; filename=\"a_b_c.mp3\""));
    }

    #[test]
    fn stream_options_validate_their_inputs() {
        let q = StreamQuery {
            url: "https://soundcloud.com/a/b".into(),
            format: Some("flac".into()),
            protocol: None,
            no_direct: false,
        };
        assert_eq!(q.to_options().unwrap_err().status, StatusCode::BAD_REQUEST);

        let q = StreamQuery {
            url: "https://soundcloud.com/a/b".into(),
            format: None,
            protocol: Some("dash".into()),
            no_direct: false,
        };
        assert_eq!(q.to_options().unwrap_err().status, StatusCode::BAD_REQUEST);

        let q = StreamQuery {
            url: "https://soundcloud.com/a/b".into(),
            format: Some("opus".into()),
            protocol: Some("hls".into()),
            no_direct: true,
        };
        let opts = q.to_options().unwrap();
        assert_eq!(opts.format, Some(Format::Opus));
        assert!(!opts.use_download_link);
    }

    #[test]
    fn percent_encoding_leaves_safe_chars_alone() {
        assert_eq!(percent_encode("abc-1_2.mp3"), "abc-1_2.mp3");
        assert_eq!(percent_encode("a b"), "a%20b");
    }
}
