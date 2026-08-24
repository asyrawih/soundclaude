//! A user's likes — the one endpoint that needs real pagination.
//!
//! `/users/{id}/likes` returns at most 200 entries per page and hands back a
//! `next_href` to continue with. Entries can be tracks *or* playlists; the node
//! package kept only tracks, and so does [`Likes::tracks`].

use crate::model::{Set, Track};
use serde::{Deserialize, Serialize};

/// The largest page `SoundCloud` will serve for this endpoint.
pub const MAX_PAGE_SIZE: u32 = 200;

/// Hard stop on how many pages one walk will fetch.
///
/// Filtering to tracks means a page can contribute nothing toward the requested
/// limit (a run of liked playlists), so the walk is not bounded by `limit` alone.
/// At 200 entries a page this still covers 100k likes.
pub const MAX_PAGES: usize = 500;

/// One entry in a user's likes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Like {
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub kind: String,
    /// Present when the liked resource is a track.
    #[serde(default)]
    pub track: Option<Track>,
    /// Present when the liked resource is a playlist or album.
    #[serde(default)]
    pub playlist: Option<Set>,
}

impl Like {
    pub fn is_track(&self) -> bool {
        self.track.is_some()
    }
}

/// The accumulated result of walking however many pages were needed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Likes {
    pub collection: Vec<Like>,
    /// Set when more likes remain beyond what was collected.
    pub next_href: Option<String>,
    /// How many pages were actually fetched.
    pub pages_fetched: usize,
}

impl Likes {
    /// Just the liked tracks, in order, skipping liked playlists.
    pub fn tracks(&self) -> impl Iterator<Item = &Track> {
        self.collection.iter().filter_map(|l| l.track.as_ref())
    }

    pub fn len(&self) -> usize {
        self.collection.len()
    }

    pub fn is_empty(&self) -> bool {
        self.collection.is_empty()
    }
}

/// How many entries to ask for on the next request.
///
/// `remaining` of `None` means "everything", which is just the biggest page the
/// endpoint allows. Otherwise never ask for more than is still wanted, so the
/// last page doesn't drag down a few hundred entries to discard.
pub(crate) fn page_size(remaining: Option<u32>) -> u32 {
    match remaining {
        Some(n) => n.clamp(1, MAX_PAGE_SIZE),
        None => MAX_PAGE_SIZE,
    }
}

/// Whether to stop after folding in a page.
///
/// `raw_page_len` is the page as `SoundCloud` sent it, before liked playlists are
/// filtered out — a page thinned by filtering says nothing about exhaustion,
/// but an empty raw page does.
pub(crate) fn should_stop(
    remaining: Option<u32>,
    raw_page_len: usize,
    next_href: Option<&str>,
) -> bool {
    next_href.is_none() || raw_page_len == 0 || remaining == Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_never_exceeds_the_endpoint_cap() {
        assert_eq!(page_size(None), MAX_PAGE_SIZE);
        assert_eq!(page_size(Some(10)), 10);
        assert_eq!(page_size(Some(5_000)), MAX_PAGE_SIZE);
        // Asking for zero would return an empty page forever.
        assert_eq!(page_size(Some(0)), 1);
    }

    #[test]
    fn stops_when_there_is_nothing_left_to_follow() {
        assert!(should_stop(None, 200, None));
        assert!(should_stop(None, 0, Some("https://x")));
        assert!(should_stop(Some(0), 200, Some("https://x")));
        assert!(!should_stop(None, 200, Some("https://x")));
        // A page thinned by filtering out liked playlists must not end the walk.
        assert!(!should_stop(Some(50), 200, Some("https://x")));
    }

    #[test]
    fn separates_liked_tracks_from_liked_playlists() {
        let likes: Likes = Likes {
            collection: vec![
                serde_json::from_value(serde_json::json!({
                    "kind": "like",
                    "track": { "id": 1, "kind": "track", "title": "a" }
                }))
                .unwrap(),
                serde_json::from_value(serde_json::json!({
                    "kind": "like",
                    "playlist": { "id": 2, "kind": "playlist", "title": "b" }
                }))
                .unwrap(),
            ],
            next_href: None,
            pages_fetched: 1,
        };

        assert_eq!(likes.len(), 2);
        assert_eq!(likes.tracks().count(), 1);
        assert_eq!(likes.tracks().next().unwrap().id, 1);
        assert!(likes.collection[0].is_track());
        assert!(!likes.collection[1].is_track());
    }
}
