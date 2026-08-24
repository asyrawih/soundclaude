//! Deserialization targets for the api-v2.soundcloud.com payloads.
//!
//! Every struct keeps an `extra` catch-all so a field `SoundCloud` adds tomorrow
//! is still reachable instead of being silently dropped.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::fmt;

/// How `SoundCloud` ships the audio bytes for a given transcoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    /// One plain HTTP response holding the whole file.
    Progressive,
    /// A `.m3u8` playlist pointing at segments that must be concatenated.
    Hls,
    /// Something `SoundCloud` introduced that this crate doesn't model yet.
    Other(String),
}

impl Protocol {
    pub fn as_str(&self) -> &str {
        match self {
            Protocol::Progressive => "progressive",
            Protocol::Hls => "hls",
            Protocol::Other(s) => s,
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Protocol {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "progressive" => Protocol::Progressive,
            "hls" => Protocol::Hls,
            other => Protocol::Other(other.to_string()),
        })
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(s.parse().unwrap_or(Protocol::Other(s)))
    }
}

impl Serialize for Protocol {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Audio codec/container of a transcoding, derived from its mime type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Format {
    /// `audio/mpeg`
    Mp3,
    /// `audio/ogg; codecs="opus"`
    Opus,
    /// `audio/mp4; codecs="mp4a.40.2"`
    Aac,
    Other(String),
}

impl Format {
    /// Classify a raw `format.mime_type` string.
    pub fn from_mime(mime: &str) -> Self {
        let m = mime.to_ascii_lowercase();
        // `audio/mpegurl` is an m3u8 playlist, not mpeg audio — and it shares a
        // prefix with `audio/mpeg`, so it has to be ruled out first.
        if m.starts_with("audio/mpegurl") || m.contains("mpegurl") {
            Format::Other(mime.to_string())
        } else if m.starts_with("audio/mpeg") {
            Format::Mp3
        } else if m.contains("opus") {
            Format::Opus
        } else if m.starts_with("audio/mp4") || m.contains("mp4a") || m.contains("aac") {
            Format::Aac
        } else {
            Format::Other(mime.to_string())
        }
    }

    /// The mime type `SoundCloud` uses for this format.
    pub fn mime(&self) -> &str {
        match self {
            Format::Mp3 => "audio/mpeg",
            Format::Opus => r#"audio/ogg; codecs="opus""#,
            Format::Aac => r#"audio/mp4; codecs="mp4a.40.2""#,
            Format::Other(s) => s,
        }
    }

    /// File extension to use when writing this format to disk.
    ///
    /// HLS opus/aac segments are raw elementary streams, so the extension is a
    /// best-effort label rather than a promise of a well-formed container.
    pub fn extension(&self) -> &str {
        match self {
            Format::Mp3 => "mp3",
            Format::Opus => "opus",
            Format::Aac => "m4a",
            Format::Other(_) => "audio",
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Format::Mp3 => f.write_str("mp3"),
            Format::Opus => f.write_str("opus"),
            Format::Aac => f.write_str("aac"),
            Format::Other(s) => f.write_str(s),
        }
    }
}

impl std::str::FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "mp3" | "mpeg" | "audio/mpeg" => Format::Mp3,
            "opus" | "ogg" => Format::Opus,
            "aac" | "m4a" | "mp4" => Format::Aac,
            other if other.contains('/') => Format::from_mime(other),
            other => return Err(format!("unknown audio format: {other}")),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TranscodingFormat {
    pub protocol: Protocol,
    pub mime_type: String,
}

/// One playable rendition of a track.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Transcoding {
    /// Not the audio itself — a lookup endpoint that returns the real CDN url.
    pub url: String,
    #[serde(default)]
    pub preset: String,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub snipped: bool,
    #[serde(default)]
    pub quality: Option<String>,
    pub format: TranscodingFormat,
}

impl Transcoding {
    pub fn format(&self) -> Format {
        Format::from_mime(&self.format.mime_type)
    }

    pub fn protocol(&self) -> &Protocol {
        &self.format.protocol
    }

    pub(crate) fn is_usable(&self) -> bool {
        !self.url.is_empty() && !self.format.mime_type.is_empty()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Media {
    #[serde(default)]
    pub transcodings: Vec<Transcoding>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    pub id: u64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub permalink: Option<String>,
    #[serde(default)]
    pub permalink_url: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub followers_count: Option<u64>,
    #[serde(default)]
    pub followings_count: Option<u64>,
    #[serde(default)]
    pub track_count: Option<u64>,
    #[serde(default)]
    pub verified: Option<bool>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// A `SoundCloud` track. Most fields are optional because `/tracks?ids=` and
/// `/resolve` return different subsets, and playlist payloads are sparser still.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Track {
    pub id: u64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub permalink: Option<String>,
    #[serde(default)]
    pub permalink_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub full_duration: Option<u64>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub tag_list: Option<String>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub waveform_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub display_date: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub label_name: Option<String>,
    #[serde(default)]
    pub monetization_model: Option<String>,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub downloadable: bool,
    #[serde(default)]
    pub has_downloads_left: bool,
    #[serde(default)]
    pub download_count: Option<u64>,
    #[serde(default)]
    pub playback_count: Option<u64>,
    #[serde(default)]
    pub likes_count: Option<u64>,
    #[serde(default)]
    pub reposts_count: Option<u64>,
    #[serde(default)]
    pub comment_count: Option<u64>,
    #[serde(default)]
    pub streamable: Option<bool>,
    #[serde(default)]
    pub public: Option<bool>,
    #[serde(default)]
    pub sharing: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub user_id: Option<u64>,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub media: Option<Media>,
    #[serde(default)]
    pub publisher_metadata: Option<Value>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

impl Track {
    /// A track fetched from a playlist payload may be a stub (id only) until it
    /// is hydrated via `/tracks?ids=`.
    pub fn is_hydrated(&self) -> bool {
        self.title.is_some()
    }

    pub fn transcodings(&self) -> &[Transcoding] {
        self.media
            .as_ref()
            .map_or(&[], |m| m.transcodings.as_slice())
    }

    /// `Artist - Title`, falling back to the track id when the payload is sparse.
    pub fn display_name(&self) -> String {
        match (
            self.user.as_ref().map(|u| u.username.as_str()),
            self.title.as_deref(),
        ) {
            (Some(artist), Some(title)) if !artist.is_empty() => format!("{artist} - {title}"),
            (_, Some(title)) => title.to_string(),
            _ => format!("track-{}", self.id),
        }
    }
}

/// A playlist or album.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Set {
    pub id: u64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub permalink: Option<String>,
    #[serde(default)]
    pub permalink_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub tag_list: Option<String>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub track_count: Option<u64>,
    #[serde(default)]
    pub set_type: Option<String>,
    #[serde(default)]
    pub is_album: bool,
    #[serde(default)]
    pub public: Option<bool>,
    #[serde(default)]
    pub sharing: Option<String>,
    #[serde(default)]
    pub secret_token: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub likes_count: Option<u64>,
    #[serde(default)]
    pub reposts_count: Option<u64>,
    #[serde(default)]
    pub user_id: Option<u64>,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// The generic `{ collection, next_href, ... }` envelope api-v2 uses everywhere.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PaginatedQuery<T> {
    #[serde(default = "Vec::new")]
    pub collection: Vec<T>,
    /// Omitted by `SoundCloud` whenever a `limit` parameter was supplied.
    #[serde(default)]
    pub total_results: Option<u64>,
    #[serde(default)]
    pub next_href: Option<String>,
    #[serde(default)]
    pub query_urn: Option<String>,
}

/// Whatever `/resolve` hands back — the `kind` field discriminates.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Resolved {
    Track(Box<Track>),
    Playlist(Box<Set>),
    User(Box<User>),
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_classification_covers_the_presets_soundcloud_ships() {
        assert_eq!(Format::from_mime("audio/mpeg"), Format::Mp3);
        assert_eq!(
            Format::from_mime(r#"audio/ogg; codecs="opus""#),
            Format::Opus
        );
        assert_eq!(
            Format::from_mime(r#"audio/mp4; codecs="mp4a.40.2""#),
            Format::Aac
        );
        // The `abr_sq` preset advertises a playlist mime that merely *looks* like mp3.
        assert!(matches!(
            Format::from_mime("audio/mpegurl"),
            Format::Other(_)
        ));
        assert!(matches!(
            Format::from_mime("application/vnd.apple.mpegurl"),
            Format::Other(_)
        ));
    }

    #[test]
    fn protocol_round_trips_unknown_values() {
        let p: Protocol = "quic-magic".parse().unwrap();
        assert_eq!(p.as_str(), "quic-magic");
        assert_eq!(
            serde_json::from_str::<Protocol>("\"hls\"").unwrap(),
            Protocol::Hls
        );
    }

    #[test]
    fn format_parses_user_input() {
        assert_eq!("mp3".parse::<Format>().unwrap(), Format::Mp3);
        assert_eq!("m4a".parse::<Format>().unwrap(), Format::Aac);
        assert!("flac".parse::<Format>().is_err());
    }
}
