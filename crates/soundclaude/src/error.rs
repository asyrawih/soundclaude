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

    #[error("no transcoding matched the requested {0}")]
    NoMatchingTranscoding(Requested),

    #[error("soundcloud returned no media url for {0}")]
    MissingMediaUrl(String),

    #[error("hls: {0}")]
    Hls(String),

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
    pub fn is_auth(&self) -> bool {
        matches!(self, Error::Unauthorized { .. })
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Unauthorized { .. } => Some(401),
            Error::NotFound { .. } => Some(404),
            Error::Status { status, .. } => Some(*status),
            Error::Http(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }
}
