//! Track and set metadata lookups against api-v2.

use crate::error::{Error, Result};
use crate::model::{Set, Track};
use crate::url as scurl;

pub(crate) const API: &str = "https://api-v2.soundcloud.com";
pub(crate) const RESOLVE: &str = "https://api-v2.soundcloud.com/resolve";

/// `/tracks?ids=` accepts at most this many ids per request.
pub(crate) const ID_BATCH_SIZE: usize = 50;

/// Playlist payloads inline the first few tracks in full and leave the rest as
/// `{ id }` stubs. Hydrate the stubs, then restore the original playlist order.
pub(crate) fn merge_hydrated(mut original: Vec<Track>, hydrated: Vec<Track>) -> Vec<Track> {
    if hydrated.is_empty() {
        return original;
    }

    let mut by_id: std::collections::HashMap<u64, Track> =
        hydrated.into_iter().map(|t| (t.id, t)).collect();

    for slot in &mut original {
        if let Some(full) = by_id.remove(&slot.id) {
            *slot = full;
        }
    }
    original
}

/// Split ids into `/tracks?ids=` sized chunks.
pub(crate) fn batches(ids: &[u64]) -> Vec<Vec<u64>> {
    ids.chunks(ID_BATCH_SIZE).map(|c| c.to_vec()).collect()
}

pub(crate) fn tracks_by_id_url(
    ids: &[u64],
    client_id: &str,
    playlist_id: Option<u64>,
    playlist_secret_token: Option<&str>,
) -> Result<String> {
    let joined = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");

    let mut url = scurl::append_query(
        &format!("{API}/tracks"),
        &[("ids", joined.as_str()), ("client_id", client_id)],
    )?;

    // Private playlists need both the id and the token for their tracks to resolve.
    if let (Some(id), Some(token)) = (playlist_id, playlist_secret_token) {
        url = scurl::append_query(
            &url,
            &[
                ("playlistId", id.to_string().as_str()),
                ("playlistSecretToken", token),
            ],
        )?;
    }
    Ok(url)
}

pub(crate) fn resolve_url(target: &str, client_id: &str) -> Result<String> {
    scurl::append_query(RESOLVE, &[("url", target), ("client_id", client_id)])
}

/// A resolved payload only counts as a track once it carries playable media.
pub(crate) fn ensure_track(track: Track, source: &str) -> Result<Track> {
    if track.media.is_none() {
        return Err(Error::NotATrack(source.to_string()));
    }
    Ok(track)
}

pub(crate) fn ensure_set(set: Set, source: &str) -> Result<Set> {
    if set.tracks.is_empty() && set.track_count.unwrap_or(0) > 0 {
        return Err(Error::NotASet(source.to_string()));
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub(id: u64) -> Track {
        serde_json::from_value(serde_json::json!({ "id": id, "kind": "track" })).unwrap()
    }

    fn full(id: u64, title: &str) -> Track {
        serde_json::from_value(serde_json::json!({
            "id": id, "kind": "track", "title": title,
            "media": { "transcodings": [] }
        }))
        .unwrap()
    }

    #[test]
    fn hydration_keeps_playlist_order() {
        let original = vec![stub(3), full(1, "already here"), stub(2)];
        // The api returns hydrated tracks in whatever order it likes.
        let hydrated = vec![full(2, "two"), full(3, "three")];

        let merged = merge_hydrated(original, hydrated);
        assert_eq!(
            merged.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
        assert_eq!(merged[0].title.as_deref(), Some("three"));
        assert_eq!(merged[1].title.as_deref(), Some("already here"));
        assert_eq!(merged[2].title.as_deref(), Some("two"));
    }

    #[test]
    fn hydration_tolerates_missing_ids() {
        let merged = merge_hydrated(vec![stub(1), stub(2)], vec![full(2, "two")]);
        assert!(!merged[0].is_hydrated());
        assert!(merged[1].is_hydrated());
    }

    #[test]
    fn batching_respects_the_api_limit() {
        let ids: Vec<u64> = (0..125).collect();
        let chunks = batches(&ids);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 50);
        assert_eq!(chunks[2].len(), 25);
        assert!(batches(&[]).is_empty());
    }

    #[test]
    fn track_url_includes_playlist_credentials_only_when_complete() {
        let plain = tracks_by_id_url(&[1, 2], "cid", None, None).unwrap();
        assert!(plain.contains("ids=1%2C2"));
        assert!(!plain.contains("playlistId"));

        let private = tracks_by_id_url(&[1], "cid", Some(9), Some("tok")).unwrap();
        assert!(private.contains("playlistId=9") && private.contains("playlistSecretToken=tok"));

        let half = tracks_by_id_url(&[1], "cid", Some(9), None).unwrap();
        assert!(!half.contains("playlistId"));
    }
}
