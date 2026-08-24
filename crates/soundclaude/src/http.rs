//! Shared request plumbing: browser-ish headers and status -> `Error` mapping.

use crate::error::{Error, Result};

pub(crate) const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0.0.0 Safari/537.36";

/// Turn a non-2xx response into a typed error, keeping a snippet of the body for context.
pub(crate) async fn check(res: reqwest::Response) -> Result<reqwest::Response> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }

    let url = res.url().to_string();
    match status.as_u16() {
        401 | 403 => Err(Error::Unauthorized { url }),
        404 => Err(Error::NotFound { url }),
        status => {
            let body = res.text().await.unwrap_or_default();
            let body: String = body.chars().take(300).collect();
            Err(Error::Status { status, url, body })
        }
    }
}

pub(crate) fn build_client() -> Result<reqwest::Client> {
    use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE};

    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));

    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .build()?)
}
