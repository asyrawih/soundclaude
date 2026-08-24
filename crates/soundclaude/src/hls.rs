//! A minimal HLS reader — enough for `SoundCloud`, and nothing more.
//!
//! This replaces node's `m3u8stream`. `SoundCloud` serves unencrypted VOD playlists
//! whose segments are plain MP3 frames / raw Opus / AAC, so "playing" the stream is
//! just concatenating the segments in order. Segments are fetched with bounded
//! look-ahead concurrency and emitted strictly in playlist order.

use crate::error::{Error, Result};
use crate::http;
use bytes::Bytes;
use futures_util::stream::{Stream, StreamExt};
use url::Url;

/// How many segments to keep in flight when no explicit concurrency is given.
pub const DEFAULT_CONCURRENCY: usize = 6;

/// `SoundCloud` playlists are small; this guards against a pathological response.
const MAX_SEGMENTS: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Playlist {
    /// `#EXT-X-STREAM-INF` variants pointing at further playlists.
    Master(Vec<Url>),
    /// Actual media segments, in playback order.
    Media(Vec<Url>),
}

/// Parse an m3u8 body, resolving every URI against `base`.
pub(crate) fn parse(body: &str, base: &Url) -> Result<Playlist> {
    if !body.trim_start().starts_with("#EXTM3U") {
        return Err(Error::Hls("response is not an m3u8 playlist".into()));
    }

    let mut is_master = false;
    let mut uris: Vec<Url> = Vec::new();

    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(tag) = line.strip_prefix('#') {
            if tag.starts_with("EXT-X-STREAM-INF") {
                is_master = true;
            } else if tag.starts_with("EXT-X-KEY") && !tag.contains("METHOD=NONE") {
                // AES-128 / SAMPLE-AES would need decryption before concatenation.
                return Err(Error::Hls(
                    "encrypted playlist (EXT-X-KEY) is not supported".into(),
                ));
            } else if tag.starts_with("EXT-X-MAP") {
                // fMP4 playlists (SoundCloud's AAC presets) put the moov atom in a
                // separate init segment. Concatenating it ahead of the media
                // segments is exactly what a player does, and yields a valid file.
                let uri = attribute(tag, "URI")
                    .ok_or_else(|| Error::Hls("EXT-X-MAP is missing its URI attribute".into()))?;
                let resolved = base
                    .join(&uri)
                    .map_err(|e| Error::Hls(format!("bad EXT-X-MAP uri `{uri}`: {e}")))?;
                if uris.is_empty() {
                    uris.push(resolved);
                } else {
                    // A mid-playlist EXT-X-MAP would mean the container changes
                    // partway through, which plain concatenation cannot express.
                    return Err(Error::Hls(
                        "playlist changes its EXT-X-MAP mid-stream, which is not supported".into(),
                    ));
                }
            }
            continue;
        }

        let resolved = base
            .join(line)
            .map_err(|e| Error::Hls(format!("bad segment uri `{line}`: {e}")))?;
        uris.push(resolved);

        if uris.len() > MAX_SEGMENTS {
            return Err(Error::Hls(format!(
                "playlist has more than {MAX_SEGMENTS} segments, refusing to continue"
            )));
        }
    }

    if uris.is_empty() {
        return Err(Error::Hls("playlist contains no segments".into()));
    }

    Ok(if is_master {
        Playlist::Master(uris)
    } else {
        Playlist::Media(uris)
    })
}

/// Read a quoted attribute out of an m3u8 tag body, e.g. `URI="init.mp4"`.
fn attribute(tag: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Fetch a playlist url and return its segment list, following one level of
/// master -> media indirection.
pub(crate) async fn segments(client: &reqwest::Client, playlist_url: &str) -> Result<Vec<Url>> {
    let (body, base) = fetch_playlist(client, playlist_url).await?;

    match parse(&body, &base)? {
        Playlist::Media(segs) => Ok(segs),
        Playlist::Master(variants) => {
            // SoundCloud only ever exposes one variant per transcoding.
            let first = variants
                .first()
                .ok_or_else(|| Error::Hls("master playlist has no variants".into()))?;
            let (body, base) = fetch_playlist(client, first.as_str()).await?;
            match parse(&body, &base)? {
                Playlist::Media(segs) => Ok(segs),
                Playlist::Master(_) => Err(Error::Hls(
                    "master playlist points at another master playlist".into(),
                )),
            }
        }
    }
}

async fn fetch_playlist(client: &reqwest::Client, url: &str) -> Result<(String, Url)> {
    let res = client.get(url).send().await?;
    let res = http::check(res).await?;
    // Redirects mean the effective url — not the requested one — is the right base.
    let base = res.url().clone();
    let body = res.text().await?;
    Ok((body, base))
}

/// Stream the segments in order, keeping `concurrency` requests in flight.
///
/// `buffered` preserves ordering, so the concatenated output is still correct.
pub(crate) fn segment_stream(
    client: reqwest::Client,
    segments: Vec<Url>,
    concurrency: usize,
) -> impl Stream<Item = Result<Bytes>> + Send + 'static {
    futures_util::stream::iter(segments)
        .map(move |segment| {
            let client = client.clone();
            async move {
                let res = client.get(segment.clone()).send().await?;
                let res = http::check(res).await?;
                Ok(res.bytes().await?)
            }
        })
        .buffered(concurrency.clamp(1, 32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cf-hls-media.sndcdn.com/playlist/track.128.mp3/playlist.m3u8").unwrap()
    }

    #[test]
    fn parses_a_media_playlist_and_resolves_relative_uris() {
        let body = "#EXTM3U\n#EXT-X-VERSION:6\n#EXTINF:10.0,\nseg0.mp3\n#EXTINF:10.0,\nhttps://cdn.example.com/seg1.mp3\n#EXT-X-ENDLIST\n";
        let Playlist::Media(segs) = parse(body, &base()).unwrap() else {
            panic!("expected a media playlist");
        };
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs[0].as_str(),
            "https://cf-hls-media.sndcdn.com/playlist/track.128.mp3/seg0.mp3"
        );
        assert_eq!(segs[1].as_str(), "https://cdn.example.com/seg1.mp3");
    }

    #[test]
    fn detects_master_playlists() {
        let body = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000\nmedia.m3u8\n";
        assert!(matches!(parse(body, &base()).unwrap(), Playlist::Master(v) if v.len() == 1));
    }

    #[test]
    fn prepends_the_fmp4_init_segment() {
        let body = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:10,\nseg0.m4s\n#EXT-X-ENDLIST\n";
        let Playlist::Media(segs) = parse(body, &base()).unwrap() else {
            panic!("expected a media playlist");
        };
        assert_eq!(segs.len(), 2);
        assert!(segs[0].as_str().ends_with("/init.mp4"));
        assert!(segs[1].as_str().ends_with("/seg0.m4s"));
    }

    #[test]
    fn rejects_a_mid_stream_container_switch() {
        let body =
            "#EXTM3U\n#EXTINF:10,\nseg0.m4s\n#EXT-X-MAP:URI=\"init2.mp4\"\n#EXTINF:10,\nseg1.m4s\n";
        assert!(parse(body, &base()).is_err());
    }

    #[test]
    fn reads_quoted_tag_attributes() {
        assert_eq!(
            attribute("EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"1@0\"", "URI").as_deref(),
            Some("init.mp4")
        );
        assert_eq!(attribute("EXT-X-MAP:BYTERANGE=\"1@0\"", "URI"), None);
    }

    #[test]
    fn rejects_encrypted_and_malformed_playlists() {
        let encrypted = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:10,\nseg0.mp3\n";
        assert!(parse(encrypted, &base()).is_err());

        let none_key = "#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:10,\nseg0.mp3\n";
        assert!(parse(none_key, &base()).is_ok());

        assert!(parse("not a playlist", &base()).is_err());
        assert!(parse("#EXTM3U\n#EXT-X-ENDLIST\n", &base()).is_err());
    }
}
