//! Mapping library errors onto HTTP responses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Wrapper so handlers can `?` on `soundclaude::Error`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // 5xx means we broke; 4xx means the request did. Only log the former loudly.
        if self.status.is_server_error() {
            tracing::error!(kind = self.kind, message = %self.message, "request failed");
        } else {
            tracing::debug!(kind = self.kind, message = %self.message, "request rejected");
        }

        let body = Json(json!({
            "error": { "kind": self.kind, "message": self.message }
        }));
        (self.status, body).into_response()
    }
}

impl From<soundclaude::Error> for ApiError {
    fn from(err: soundclaude::Error) -> Self {
        use soundclaude::Error as E;

        let (status, kind) = match &err {
            E::NotSoundcloudUrl(_) | E::UrlParse(_) => (StatusCode::BAD_REQUEST, "invalid_url"),
            E::FirebaseUnresolved(_) => (StatusCode::BAD_REQUEST, "unresolvable_link"),
            E::NotATrack(_) => (StatusCode::BAD_REQUEST, "not_a_track"),
            E::NotASet(_) => (StatusCode::BAD_REQUEST, "not_a_set"),
            E::NoMatchingTranscoding(_) | E::NoTranscodings(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "no_media")
            }
            E::NotFound { .. } => (StatusCode::NOT_FOUND, "not_found"),
            // The caller's client_id is fine — ours went stale, so this is on us.
            E::Unauthorized { .. } => (StatusCode::BAD_GATEWAY, "upstream_unauthorized"),
            E::ClientIdNotFound => (StatusCode::BAD_GATEWAY, "client_id_unavailable"),
            // The operator configured SOUNDCLOUD_CLIENT_ID with a value soundcloud
            // refuses. That is this deployment being misconfigured, not upstream
            // being down and not the caller's request being wrong.
            E::ClientIdRejected => (StatusCode::INTERNAL_SERVER_ERROR, "client_id_rejected"),
            E::Status { status, .. } if (500..600).contains(status) => {
                (StatusCode::BAD_GATEWAY, "upstream_error")
            }
            E::Status { .. } | E::MissingMediaUrl(_) | E::Hls(_) => {
                (StatusCode::BAD_GATEWAY, "upstream_error")
            }
            E::Http(e) if e.is_timeout() => (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout"),
            E::Http(_) => (StatusCode::BAD_GATEWAY, "upstream_error"),
            E::Json(_) => (StatusCode::BAD_GATEWAY, "upstream_malformed_json"),
            E::Io(_) | E::Other(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };

        Self::new(status, kind, err.to_string())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_mistakes_are_4xx_and_upstream_faults_are_5xx() {
        let bad_url: ApiError = soundclaude::Error::NotSoundcloudUrl("x".into()).into();
        assert_eq!(bad_url.status, StatusCode::BAD_REQUEST);

        let missing: ApiError = soundclaude::Error::NotFound { url: "x".into() }.into();
        assert_eq!(missing.status, StatusCode::NOT_FOUND);

        // A rotated client_id is our problem to fix, not the caller's.
        let unauthorized: ApiError = soundclaude::Error::Unauthorized { url: "x".into() }.into();
        assert_eq!(unauthorized.status, StatusCode::BAD_GATEWAY);

        let hls: ApiError = soundclaude::Error::Hls("boom".into()).into();
        assert_eq!(hls.status, StatusCode::BAD_GATEWAY);

        // A bad configured client_id is our misconfiguration, so neither 4xx nor 502.
        let rejected: ApiError = soundclaude::Error::ClientIdRejected.into();
        assert_eq!(rejected.status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
