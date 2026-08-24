//! # soundclaude
//!
//! A `SoundCloud` client and audio downloader in pure Rust — a port of
//! [`node-soundcloud-downloader`](https://github.com/zackradisic/node-soundcloud-downloader).
//!
//! It scrapes a public `client_id` the way the web player uses one, resolves
//! tracks and playlists through `api-v2.soundcloud.com`, and streams audio over
//! either delivery protocol `SoundCloud` offers: `progressive` (one HTTP response)
//! or `hls` (an m3u8 playlist whose segments are fetched concurrently and
//! concatenated in order). No ffmpeg, no other external binaries.
//!
//! ```no_run
//! # async fn example() -> Result<(), soundclaude::Error> {
//! let scdl = soundclaude::Client::new()?;
//!
//! let track = scdl.track("https://soundcloud.com/artist/track").await?;
//! println!("{} ({} ms)", track.display_name(), track.duration.unwrap_or(0));
//!
//! let audio = scdl.download("https://soundcloud.com/artist/track").await?;
//! audio.save("out.mp3").await?;
//! # Ok(())
//! # }
//! ```
//!
//! Downloading is subject to `SoundCloud`'s terms of service and to the rights of
//! whoever made the track; this crate is a transport, not a licence.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod client;
pub mod client_id;
pub mod download;
pub mod error;
pub mod hls;
pub mod likes;
pub mod model;
pub mod url;

mod http;
mod info;

pub use client::{Client, ClientBuilder, Options};
pub use download::{filter_media, sanitize, suggested_filename, AudioStream, DownloadOptions};
pub use error::{Error, Result};
pub use likes::{Like, Likes};
pub use model::{Format, Media, PaginatedQuery, Protocol, Resolved, Set, Track, Transcoding, User};
pub use url::{
    is_firebase_url, is_personalized_track_url, is_playlist_url, is_valid_url, strip_mobile_prefix,
};
