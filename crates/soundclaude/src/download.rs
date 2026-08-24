//! Turning a `Transcoding` into actual bytes.

use crate::error::{Error, Requested, Result};
use crate::model::{Format, Protocol, Track, Transcoding};
use bytes::Bytes;
use futures_util::stream::{BoxStream, Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

/// What to download and how.
#[derive(Debug, Clone, Default)]
pub struct DownloadOptions {
    /// Only accept this audio format.
    pub format: Option<Format>,
    /// Only accept this delivery protocol.
    pub protocol: Option<Protocol>,
    /// Try the artist's original-file download endpoint first when the track
    /// allows it. Some tracks advertise it and then 404, so failures fall back
    /// to the normal transcodings.
    pub use_download_link: bool,
    /// Segments in flight for HLS. `None` uses [`crate::hls::DEFAULT_CONCURRENCY`].
    pub hls_concurrency: Option<usize>,
}

impl DownloadOptions {
    pub fn new() -> Self {
        Self {
            use_download_link: true,
            ..Default::default()
        }
    }

    #[must_use]
    pub fn format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
    }

    #[must_use]
    pub fn protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = Some(protocol);
        self
    }

    #[must_use]
    pub fn use_download_link(mut self, yes: bool) -> Self {
        self.use_download_link = yes;
        self
    }

    #[must_use]
    pub fn hls_concurrency(mut self, n: usize) -> Self {
        self.hls_concurrency = Some(n);
        self
    }

    pub(crate) fn requested(&self) -> Requested {
        Requested {
            format: self.format.as_ref().map(|f| f.to_string()),
            protocol: self.protocol.as_ref().map(|p| p.to_string()),
        }
    }
}

/// Pick the transcoding to download.
///
/// Explicit `format`/`protocol` filters are hard requirements. Among whatever
/// survives, progressive beats HLS (one request, exact byte length) and mp3 beats
/// the rest (it lands in a real container without remuxing).
pub(crate) fn select<'a>(
    transcodings: &'a [Transcoding],
    opts: &DownloadOptions,
) -> Result<&'a Transcoding> {
    let matching: Vec<&Transcoding> = transcodings
        .iter()
        .filter(|t| t.is_usable())
        .filter(|t| opts.format.as_ref().is_none_or(|want| &t.format() == want))
        .filter(|t| {
            opts.protocol
                .as_ref()
                .is_none_or(|want| t.protocol() == want)
        })
        .collect();

    if matching.is_empty() {
        return Err(if transcodings.is_empty() {
            Error::NoTranscodings("track has an empty media.transcodings list".into())
        } else {
            Error::NoMatchingTranscoding(opts.requested())
        });
    }

    // Lower score wins.
    let score = |t: &Transcoding| -> (u8, u8) {
        let protocol_rank = match t.protocol() {
            Protocol::Progressive => 0,
            Protocol::Hls => 1,
            Protocol::Other(_) => 2,
        };
        let format_rank = match t.format() {
            Format::Mp3 => 0,
            Format::Aac => 1,
            Format::Opus => 2,
            Format::Other(_) => 3,
        };
        (protocol_rank, format_rank)
    };

    Ok(matching
        .into_iter()
        .min_by_key(|t| score(t))
        .expect("non-empty after the emptiness check"))
}

/// Filter transcodings by an optional format/protocol predicate.
///
/// The direct analogue of the node package's `filterMedia`.
pub fn filter_media<'a>(
    transcodings: &'a [Transcoding],
    format: Option<&Format>,
    protocol: Option<&Protocol>,
) -> Vec<&'a Transcoding> {
    transcodings
        .iter()
        .filter(|t| format.is_none_or(|want| &t.format() == want))
        .filter(|t| protocol.is_none_or(|want| t.protocol() == want))
        .collect()
}

/// Cap on how much is pre-allocated from an upstream `Content-Length`.
///
/// That header comes from a server we do not control, so it is a hint, not a
/// promise. Past this the buffer just grows as bytes arrive.
const MAX_PREALLOC: u64 = 32 * 1024 * 1024;

/// An in-progress audio download: a byte stream plus what is known about it.
pub struct AudioStream {
    /// Mime type as reported by `SoundCloud`, or by the CDN for original-file downloads.
    pub mime_type: String,
    /// `None` for HLS, where the total size is not known until every segment lands.
    pub content_length: Option<u64>,
    /// How the bytes are being delivered.
    pub protocol: Protocol,
    /// Suggested file name, derived from the track title, without a directory.
    pub filename: String,
    inner: BoxStream<'static, Result<Bytes>>,
}

impl AudioStream {
    pub(crate) fn new(
        mime_type: String,
        content_length: Option<u64>,
        protocol: Protocol,
        filename: String,
        inner: BoxStream<'static, Result<Bytes>>,
    ) -> Self {
        Self {
            mime_type,
            content_length,
            protocol,
            filename,
            inner,
        }
    }

    pub fn format(&self) -> Format {
        Format::from_mime(&self.mime_type)
    }

    /// Collect the whole stream into memory. Prefer [`AudioStream::write_to`] or
    /// [`AudioStream::save`] for anything long.
    pub async fn bytes(mut self) -> Result<Bytes> {
        let hint = self.content_length.unwrap_or(0).min(MAX_PREALLOC);
        let mut buf = Vec::with_capacity(usize::try_from(hint).unwrap_or(0));
        while let Some(chunk) = self.inner.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(Bytes::from(buf))
    }

    /// Pump the stream into any async writer, reporting bytes written so far.
    pub async fn write_to<W>(
        mut self,
        writer: &mut W,
        mut on_progress: impl FnMut(u64),
    ) -> Result<u64>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt;

        let mut written = 0u64;
        while let Some(chunk) = self.inner.next().await {
            let chunk = chunk?;
            writer.write_all(&chunk).await?;
            written += chunk.len() as u64;
            on_progress(written);
        }
        writer.flush().await?;
        Ok(written)
    }

    /// Write the stream to a file path.
    pub async fn save(self, path: impl AsRef<std::path::Path>) -> Result<u64> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let mut file = tokio::fs::File::create(path).await?;
        self.write_to(&mut file, |_| {}).await
    }

    /// Consume the wrapper and keep only the raw byte stream.
    pub fn into_stream(self) -> BoxStream<'static, Result<Bytes>> {
        self.inner
    }
}

impl Stream for AudioStream {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl std::fmt::Debug for AudioStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioStream")
            .field("mime_type", &self.mime_type)
            .field("content_length", &self.content_length)
            .field("protocol", &self.protocol)
            .field("filename", &self.filename)
            .finish_non_exhaustive()
    }
}

/// Build a filesystem-safe `Artist - Title.ext` for a track.
pub fn suggested_filename(track: &Track, format: &Format) -> String {
    format!("{}.{}", sanitize(&track.display_name()), format.extension())
}

/// Strip path separators, control characters, and anything Windows rejects.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();

    // Trailing dots and spaces are illegal in Windows file names.
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    let trimmed = if trimmed.is_empty() { "track" } else { trimmed };

    // Keep well under the common 255-byte name limit, on a char boundary.
    trimmed.chars().take(180).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcoding(protocol: &str, mime: &str) -> Transcoding {
        serde_json::from_value(serde_json::json!({
            "url": format!("https://api-v2.soundcloud.com/media/{protocol}/{mime}"),
            "preset": "preset",
            "snipped": false,
            "format": { "protocol": protocol, "mime_type": mime }
        }))
        .unwrap()
    }

    fn all() -> Vec<Transcoding> {
        vec![
            transcoding("hls", "audio/mpeg"),
            transcoding("hls", r#"audio/ogg; codecs="opus""#),
            transcoding("progressive", "audio/mpeg"),
        ]
    }

    #[test]
    fn prefers_progressive_mp3_by_default() {
        let media = all();
        let picked = select(&media, &DownloadOptions::new()).unwrap();
        assert_eq!(picked.protocol(), &Protocol::Progressive);
        assert_eq!(picked.format(), Format::Mp3);
    }

    #[test]
    fn honours_explicit_filters() {
        let media = all();

        let opts = DownloadOptions::new().format(Format::Opus);
        let picked = select(&media, &opts).unwrap();
        assert_eq!(picked.format(), Format::Opus);
        assert_eq!(picked.protocol(), &Protocol::Hls);

        let opts = DownloadOptions::new().protocol(Protocol::Hls);
        assert_eq!(select(&media, &opts).unwrap().protocol(), &Protocol::Hls);
    }

    #[test]
    fn falls_back_to_hls_when_progressive_is_absent() {
        let hls_only = vec![transcoding("hls", "audio/mpeg")];
        assert_eq!(
            select(&hls_only, &DownloadOptions::new())
                .unwrap()
                .protocol(),
            &Protocol::Hls
        );
    }

    #[test]
    fn reports_why_selection_failed() {
        let err = select(&[], &DownloadOptions::new()).unwrap_err();
        assert!(matches!(err, Error::NoTranscodings(_)));

        let media = all();
        let opts = DownloadOptions::new().format(Format::Aac);
        let err = select(&media, &opts).unwrap_err();
        assert!(err.to_string().contains("aac"));
    }

    #[test]
    fn skips_transcodings_missing_a_url() {
        let mut broken = transcoding("progressive", "audio/mpeg");
        broken.url = String::new();
        let media = [broken, transcoding("hls", "audio/mpeg")];
        let picked = select(&media, &DownloadOptions::new()).unwrap();
        assert_eq!(picked.protocol(), &Protocol::Hls);
    }

    #[test]
    fn filter_media_matches_the_node_semantics() {
        let media = all();
        assert_eq!(filter_media(&media, Some(&Format::Mp3), None).len(), 2);
        assert_eq!(
            filter_media(&media, None, Some(&Protocol::Progressive)).len(),
            1
        );
        assert_eq!(filter_media(&media, None, None).len(), 3);
    }

    #[test]
    fn sanitizes_hostile_track_titles() {
        assert_eq!(sanitize("a/b:c*d?"), "a_b_c_d_");
        assert_eq!(sanitize("  trailing dots... "), "trailing dots");
        assert_eq!(sanitize("   "), "track");
        assert_eq!(sanitize("é".repeat(300).as_str()).chars().count(), 180);
    }

    #[test]
    fn filenames_carry_the_right_extension() {
        let track: Track = serde_json::from_value(serde_json::json!({
            "id": 1, "kind": "track", "title": "Song/One",
            "user": { "id": 2, "username": "Artist" }
        }))
        .unwrap();
        assert_eq!(
            suggested_filename(&track, &Format::Mp3),
            "Artist - Song_One.mp3"
        );
        assert_eq!(
            suggested_filename(&track, &Format::Opus),
            "Artist - Song_One.opus"
        );
    }
}
