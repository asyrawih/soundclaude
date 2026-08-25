use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("could not parse url: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("could not parse json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Every attempt to scrape a `client_id` out of soundcloud.com's bundles failed.
    #[error("could not scrape a client_id from soundcloud.com")]
    ClientIdNotFound,

    /// A caller-supplied `client_id` was rejected. Scraping over it would silently
    /// discard explicit configuration, so it is reported instead.
    #[error("the supplied client_id was rejected by soundcloud; omit it to scrape one automatically, or enable the scrape fallback")]
    ClientIdRejected,

    #[error("unauthorized (401) for {url} — is the client_id correct?")]
    Unauthorized { url: String },

    /// Distinct from [`Error::Unauthorized`] on purpose: 401 means the `client_id` is
    /// wrong and re-scraping fixes it, while 403 means this particular resource is
    /// off limits and a new `client_id` would change nothing.
    #[error("forbidden (403) for {url} — this resource is not available to you")]
    Forbidden { url: String },

    #[error("not found (404) for {url} — the track may be private, or the url is wrong")]
    NotFound { url: String },

    #[error("unexpected status {status} from {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },

    #[error("not a soundcloud url: {0}")]
    NotSoundcloudUrl(String),

    #[error("could not resolve the firebase (soundcloud.app.goo.gl) link: {0}")]
    FirebaseUnresolved(String),

    #[error("this url does not point at a soundcloud track: {0}")]
    NotATrack(String),

    #[error("this url does not point at a soundcloud set/playlist: {0}")]
    NotASet(String),

    #[error("the track has no playable media transcodings: {0}")]
    NoTranscodings(String),

    /// The track cannot be downloaded at all — private, deleted, region blocked, or
    /// served without any media. Callers walking a playlist should skip these rather
    /// than treat them as failures.
    #[error("track {id} cannot be downloaded: {reason}")]
    TrackUnavailable {
        id: u64,
        reason: crate::model::Availability,
    },

    #[error("no transcoding matched the requested {0}")]
    NoMatchingTranscoding(Requested),

    #[error("soundcloud returned no media url for {0}")]
    MissingMediaUrl(String),

    #[error("hls: {0}")]
    Hls(String),

    /// A collection held a resource of an unexpected `kind`, which means the
    /// endpoint returned something other than what was asked for.
    #[error("expected a resource of kind `{expected}`, received `{received}`")]
    KindMismatch {
        expected: &'static str,
        received: String,
    },

    #[error("{0}")]
    Other(String),
}

/// What a caller asked for when transcoding selection came up empty.
#[derive(Debug, Clone)]
pub struct Requested {
    pub format: Option<String>,
    pub protocol: Option<String>,
}

impl fmt::Display for Requested {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.format, &self.protocol) {
            (Some(fmt_), Some(p)) => write!(f, "format `{fmt_}` over protocol `{p}`"),
            (Some(fmt_), None) => write!(f, "format `{fmt_}`"),
            (None, Some(p)) => write!(f, "protocol `{p}`"),
            (None, None) => write!(f, "media"),
        }
    }
}

impl Error {
    /// True for errors where retrying with a fresh `client_id` is worth a shot.
    ///
    /// Deliberately excludes 403: re-scraping on a forbidden track would spend a
    /// homepage fetch plus a bundle scrape per track, and still fail.
    pub fn is_auth(&self) -> bool {
        matches!(self, Error::Unauthorized { .. })
    }

    /// True when the track simply cannot be downloaded, as opposed to the attempt
    /// having gone wrong. Skip these; do not report them as failures.
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            Error::TrackUnavailable { .. } | Error::NoTranscodings(_) | Error::Forbidden { .. }
        )
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Unauthorized { .. } => Some(401),
            Error::Forbidden { .. } => Some(403),
            Error::NotFound { .. } => Some(404),
            Error::Status { status, .. } => Some(*status),
            Error::Http(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }
}
