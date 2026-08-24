//! The public entry point: a configured `SoundCloud` client.

use crate::client_id::{self, CachedClientId};
use crate::download::{self, AudioStream, DownloadOptions};
use crate::error::{Error, Result};
use crate::hls;
use crate::http;
use crate::info;
use crate::likes::{self, Like, Likes};
use crate::model::{Format, PaginatedQuery, Protocol, Resolved, Set, Track, Transcoding, User};
use crate::url as scurl;
use futures_util::stream::{StreamExt, TryStreamExt};
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Client configuration. Build one with [`Client::builder`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Use this `client_id` instead of scraping for one.
    pub client_id: Option<String>,
    /// Cache the scraped `client_id` at this path.
    pub client_id_cache: Option<PathBuf>,
    /// How long a *scraped* `client_id` stays usable. A `client_id` supplied by
    /// the caller is pinned and never expires.
    pub client_id_ttl_secs: u64,
    /// Whether a rejected caller-supplied `client_id` may be replaced by a scraped
    /// one. Off by default: silently overriding explicit configuration hides the
    /// fact that the supplied id was wrong. Has no effect on scraped ids, which
    /// are always refreshed on rejection.
    pub allow_scrape_fallback: bool,
    /// Rewrite `m.soundcloud.com` links before using them.
    pub strip_mobile_prefix: bool,
    /// Follow `soundcloud.app.goo.gl` shortlinks before using them.
    pub convert_firebase_links: bool,
    /// Segments in flight when downloading HLS.
    pub hls_concurrency: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            client_id: None,
            client_id_cache: None,
            client_id_ttl_secs: client_id::DEFAULT_TTL_SECS,
            allow_scrape_fallback: false,
            strip_mobile_prefix: true,
            convert_firebase_links: true,
            hls_concurrency: hls::DEFAULT_CONCURRENCY,
        }
    }
}

/// Fluent [`Client`] configuration.
#[derive(Debug, Default)]
pub struct ClientBuilder {
    options: Options,
    http: Option<reqwest::Client>,
}

impl ClientBuilder {
    #[must_use]
    pub fn client_id(mut self, id: impl Into<String>) -> Self {
        self.options.client_id = Some(id.into());
        self
    }

    /// Persist scraped ids here, so restarts don't re-scrape.
    #[must_use]
    pub fn cache_client_id(mut self, path: impl Into<PathBuf>) -> Self {
        self.options.client_id_cache = Some(path.into());
        self
    }

    #[must_use]
    pub fn client_id_ttl_secs(mut self, secs: u64) -> Self {
        self.options.client_id_ttl_secs = secs;
        self
    }

    /// Allow falling back to scraping when a caller-supplied `client_id` is
    /// rejected. Useful for long-running services that must stay up.
    #[must_use]
    pub fn allow_scrape_fallback(mut self, yes: bool) -> Self {
        self.options.allow_scrape_fallback = yes;
        self
    }

    #[must_use]
    pub fn strip_mobile_prefix(mut self, yes: bool) -> Self {
        self.options.strip_mobile_prefix = yes;
        self
    }

    #[must_use]
    pub fn convert_firebase_links(mut self, yes: bool) -> Self {
        self.options.convert_firebase_links = yes;
        self
    }

    #[must_use]
    pub fn hls_concurrency(mut self, n: usize) -> Self {
        self.options.hls_concurrency = n;
        self
    }

    /// Supply your own `reqwest::Client` (proxy, timeouts, connection pool).
    #[must_use]
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    pub fn build(self) -> Result<Client> {
        let http = match self.http {
            Some(h) => h,
            None => http::build_client()?,
        };
        let initial = self.options.client_id.clone().map(IdSource::Pinned);

        Ok(Client {
            http,
            options: self.options,
            client_id: Arc::new(RwLock::new(initial)),
        })
    }
}

/// Where the current `client_id` came from.
///
/// The distinction matters: a scraped id is disposable and expires, while one the
/// caller supplied is configuration and must not be swapped out behind their back.
#[derive(Debug, Clone)]
enum IdSource {
    /// Supplied by the caller. Never expires, never read from or written to the
    /// on-disk cache, never silently replaced.
    Pinned(String),
    /// Scraped from soundcloud.com, and subject to the TTL.
    Scraped(CachedClientId),
}

impl IdSource {
    fn value(&self) -> &str {
        match self {
            IdSource::Pinned(id) => id,
            IdSource::Scraped(cached) => &cached.client_id,
        }
    }

    fn is_usable(&self, ttl_secs: u64) -> bool {
        match self {
            IdSource::Pinned(id) => !id.is_empty(),
            IdSource::Scraped(cached) => cached.is_fresh(ttl_secs),
        }
    }

    fn is_pinned(&self) -> bool {
        matches!(self, IdSource::Pinned(_))
    }
}

/// A `SoundCloud` API client. Cheap to clone — clones share the connection pool
/// and the resolved `client_id`.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    options: Options,
    client_id: Arc<RwLock<Option<IdSource>>>,
}

impl Client {
    /// A client with default options, scraping its own `client_id` on first use.
    pub fn new() -> Result<Self> {
        Self::builder().build()
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    /// The underlying HTTP client, for callers that need to make their own requests.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    // ---- client_id ------------------------------------------------------

    /// The current `client_id`, scraping (and caching) one if needed.
    ///
    /// An id supplied via [`ClientBuilder::client_id`] or [`Client::set_client_id`]
    /// is returned as-is: it never expires and never triggers a scrape.
    pub async fn client_id(&self) -> Result<String> {
        if let Some(source) = self.client_id.read().await.as_ref() {
            if source.is_usable(self.options.client_id_ttl_secs) {
                return Ok(source.value().to_string());
            }
        }

        let mut slot = self.client_id.write().await;
        // Another task may have refreshed it while this one waited for the lock.
        if let Some(source) = slot.as_ref() {
            if source.is_usable(self.options.client_id_ttl_secs) {
                return Ok(source.value().to_string());
            }
        }

        // Only scraped ids go through the on-disk cache; a pinned id is
        // configuration, not something to be swapped for a previous run's value.
        if let Some(path) = &self.options.client_id_cache {
            if let Some(cached) = client_id::read_cache(path, self.options.client_id_ttl_secs).await
            {
                let id = cached.client_id.clone();
                *slot = Some(IdSource::Scraped(cached));
                return Ok(id);
            }
        }

        let fresh = client_id::fetch(&self.http).await?;
        let cached = CachedClientId::new(fresh.clone());

        if let Some(path) = &self.options.client_id_cache {
            if let Err(err) = client_id::write_cache(path, &cached).await {
                tracing::warn!(%err, "could not persist client_id cache");
            }
        }

        *slot = Some(IdSource::Scraped(cached));
        Ok(fresh)
    }

    /// Check whether a `client_id` is currently accepted by `SoundCloud`.
    ///
    /// The equivalent of `soundcloud-key-fetch`'s `keyIsValid`. Distinguishes a
    /// rejected id (`Ok(false)`) from the request itself failing (`Err`).
    pub async fn verify_client_id(&self, id: &str) -> Result<bool> {
        // A one-result search is the cheapest call that still requires a valid id
        // and does not depend on any particular track still existing.
        let url = scurl::append_query(
            &format!("{}/search/tracks", info::API),
            &[("q", "a"), ("limit", "1"), ("client_id", id)],
        )?;

        let res = self.http.get(&url).send().await?;
        match res.status().as_u16() {
            200 => Ok(true),
            401 | 403 => Ok(false),
            _ => {
                http::check(res).await?;
                Ok(true)
            }
        }
    }

    /// Drop the cached `client_id` so the next call scrapes a fresh one.
    ///
    /// A pinned id is only dropped when [`Options::allow_scrape_fallback`] is set;
    /// otherwise this leaves it in place and returns `false`.
    pub async fn invalidate_client_id(&self) -> bool {
        let mut slot = self.client_id.write().await;

        match slot.as_ref() {
            Some(source) if source.is_pinned() && !self.options.allow_scrape_fallback => false,
            _ => {
                slot.take();
                true
            }
        }
    }

    /// Pin a `client_id` at runtime. Like one given to the builder, it will not
    /// expire and will not be replaced by scraping.
    pub async fn set_client_id(&self, id: impl Into<String>) {
        *self.client_id.write().await = Some(IdSource::Pinned(id.into()));
    }

    // ---- url handling ---------------------------------------------------

    /// Normalize a user-supplied url: strip the mobile host and follow firebase
    /// shortlinks, according to this client's options.
    pub async fn prepare_url(&self, url: &str) -> Result<String> {
        let mut url = url.trim().to_string();

        if self.options.convert_firebase_links && scurl::is_firebase_url(&url) {
            url = scurl::resolve_firebase_url(&self.http, &url).await?;
        }
        if self.options.strip_mobile_prefix {
            url = scurl::strip_mobile_prefix(&url);
        }
        if !scurl::is_valid_url(&url) {
            return Err(Error::NotSoundcloudUrl(url));
        }
        Ok(url)
    }

    // ---- metadata -------------------------------------------------------

    /// GET a signed api-v2 url, retrying once with a fresh `client_id` on 401.
    async fn get_json<T: DeserializeOwned>(
        &self,
        build: impl Fn(&str) -> Result<String>,
    ) -> Result<T> {
        let url = build(&self.client_id().await?)?;
        let res = self.http.get(&url).send().await?;

        let res = match http::check(res).await {
            Ok(res) => res,
            Err(err) if err.is_auth() => {
                // A scraped id rotating out from under us is routine; retry with a
                // fresh one. A rejected *pinned* id is the caller's configuration
                // being wrong, and replacing it silently would hide that.
                if !self.invalidate_client_id().await {
                    tracing::warn!("the supplied client_id was rejected by soundcloud");
                    return Err(Error::ClientIdRejected);
                }
                tracing::debug!("client_id rejected, re-scraping and retrying once");
                let url = build(&self.client_id().await?)?;
                http::check(self.http.get(&url).send().await?).await?
            }
            Err(err) => return Err(err),
        };

        let body = res.bytes().await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// Resolve any `SoundCloud` url to whatever it points at.
    pub async fn resolve(&self, url: &str) -> Result<Resolved> {
        let prepared = self.prepare_url(url).await?;
        self.get_json(|cid| info::resolve_url(&prepared, cid)).await
    }

    /// Fetch metadata for a single track.
    pub async fn track(&self, url: &str) -> Result<Track> {
        // The discover feed hands out pseudo-urls that /resolve rejects.
        if scurl::is_personalized_track_url(url) {
            let id = scurl::extract_personalized_track_id(url)
                .ok_or_else(|| Error::NotATrack(url.to_string()))?;
            let track = self
                .tracks_by_id(&[id], None, None)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| Error::NotFound {
                    url: url.to_string(),
                })?;
            return info::ensure_track(track, url);
        }

        let prepared = self.prepare_url(url).await?;
        let track: Track = self
            .get_json(|cid| info::resolve_url(&prepared, cid))
            .await?;
        info::ensure_track(track, url)
    }

    /// Fetch metadata for tracks by id, batching to the api's 50-per-request limit.
    pub async fn tracks_by_id(
        &self,
        ids: &[u64],
        playlist_id: Option<u64>,
        playlist_secret_token: Option<&str>,
    ) -> Result<Vec<Track>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in info::batches(ids) {
            let batch: Vec<Track> = self
                .get_json(|cid| {
                    info::tracks_by_id_url(&chunk, cid, playlist_id, playlist_secret_token)
                })
                .await?;
            out.extend(batch);
        }
        Ok(out)
    }

    /// Fetch a playlist/album, hydrating the track stubs it ships with.
    pub async fn set(&self, url: &str) -> Result<Set> {
        let prepared = self.prepare_url(url).await?;
        let mut set: Set = self
            .get_json(|cid| info::resolve_url(&prepared, cid))
            .await?;

        self.hydrate_set(&mut set).await?;
        info::ensure_set(set, url)
    }

    /// Fill in a set's stub tracks in place.
    ///
    /// A playlist payload from `/resolve` inlines only the first few tracks in
    /// full; the rest arrive as bare `{ id }` objects with no title and no media.
    /// Call this on any `Set` obtained without going through [`Client::set`] —
    /// [`Client::resolve`], for instance, returns the raw payload untouched.
    ///
    /// Does nothing when every track is already hydrated.
    pub async fn hydrate_set(&self, set: &mut Set) -> Result<()> {
        let missing: Vec<u64> = set
            .tracks
            .iter()
            .filter(|t| !t.is_hydrated())
            .map(|t| t.id)
            .collect();

        if missing.is_empty() {
            return Ok(());
        }

        tracing::debug!(
            set = set.id,
            stubs = missing.len(),
            batches = missing.len().div_ceil(info::ID_BATCH_SIZE),
            "hydrating playlist tracks"
        );

        let hydrated = self
            .tracks_by_id(&missing, Some(set.id), set.secret_token.as_deref())
            .await?;
        set.tracks = info::merge_hydrated(std::mem::take(&mut set.tracks), hydrated);
        Ok(())
    }

    /// Fetch a user profile.
    pub async fn user(&self, url: &str) -> Result<User> {
        let prepared = self.prepare_url(url).await?;
        self.get_json(|cid| info::resolve_url(&prepared, cid)).await
    }

    // ---- downloading ----------------------------------------------------

    /// Resolve a transcoding's lookup endpoint to the real CDN url.
    pub async fn media_url(&self, transcoding: &Transcoding) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct MediaUrl {
            #[serde(default)]
            url: Option<String>,
        }

        let endpoint = transcoding.url.clone();
        let res: MediaUrl = self
            .get_json(|cid| scurl::append_query(&endpoint, &[("client_id", cid)]))
            .await?;

        res.url
            .filter(|u| !u.is_empty())
            .ok_or_else(|| Error::MissingMediaUrl(transcoding.url.clone()))
    }

    /// Open a byte stream for one specific transcoding.
    pub async fn stream_transcoding(
        &self,
        transcoding: &Transcoding,
        filename: String,
        hls_concurrency: Option<usize>,
    ) -> Result<AudioStream> {
        let media_url = self.media_url(transcoding).await?;
        let mime = transcoding.format.mime_type.clone();

        match transcoding.protocol() {
            Protocol::Progressive => {
                let res = http::check(self.http.get(&media_url).send().await?).await?;
                let len = res.content_length();
                let body = res.bytes_stream().map_err(Error::from).boxed();
                Ok(AudioStream::new(
                    mime,
                    len,
                    Protocol::Progressive,
                    filename,
                    body,
                ))
            }
            Protocol::Hls => {
                let segments = hls::segments(&self.http, &media_url).await?;
                tracing::debug!(segments = segments.len(), "streaming hls");
                let concurrency = hls_concurrency.unwrap_or(self.options.hls_concurrency);
                let body = hls::segment_stream(self.http.clone(), segments, concurrency).boxed();
                // Segment sizes are only known as they arrive, so no content length.
                Ok(AudioStream::new(mime, None, Protocol::Hls, filename, body))
            }
            Protocol::Other(other) => Err(Error::Hls(format!(
                "unsupported streaming protocol `{other}`"
            ))),
        }
    }

    /// Try the artist-provided original file. Returns `Ok(None)` when the track
    /// advertises a download that the api then refuses — a common `SoundCloud` quirk.
    async fn try_download_link(&self, track: &Track) -> Result<Option<AudioStream>> {
        #[derive(serde::Deserialize)]
        struct Redirect {
            #[serde(default)]
            redirect_uri: Option<String>,
        }

        let id = track.id;
        let endpoint = format!("{}/tracks/{id}/download", info::API);

        let redirect: Result<Redirect> = self
            .get_json(|cid| scurl::append_query(&endpoint, &[("client_id", cid)]))
            .await;

        let Some(uri) = redirect.ok().and_then(|r| r.redirect_uri) else {
            return Ok(None);
        };

        let res = match http::check(self.http.get(&uri).send().await?).await {
            Ok(res) => res,
            Err(err) => {
                tracing::debug!(%err, "download link failed, falling back to transcodings");
                return Ok(None);
            }
        };

        let mime = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("audio/mpeg")
            .to_string();
        let len = res.content_length();
        let format = Format::from_mime(&mime);
        let filename = download::suggested_filename(track, &format);
        let body = res.bytes_stream().map_err(Error::from).boxed();

        Ok(Some(AudioStream::new(
            mime,
            len,
            Protocol::Progressive,
            filename,
            body,
        )))
    }

    /// Download a track by url with default options.
    pub async fn download(&self, url: &str) -> Result<AudioStream> {
        self.download_with(url, &DownloadOptions::new()).await
    }

    /// Download a track by url, choosing format/protocol explicitly.
    pub async fn download_with(&self, url: &str, opts: &DownloadOptions) -> Result<AudioStream> {
        let track = self.track(url).await?;
        self.download_track(&track, opts).await
    }

    /// Download a track whose metadata is already in hand.
    pub async fn download_track(
        &self,
        track: &Track,
        opts: &DownloadOptions,
    ) -> Result<AudioStream> {
        // The original file is only worth trying when no specific format was asked for.
        if opts.use_download_link
            && track.downloadable
            && track.has_downloads_left
            && opts.format.is_none()
            && opts.protocol.is_none()
        {
            if let Some(stream) = self.try_download_link(track).await? {
                tracing::debug!(track = track.id, "using the artist's original download");
                return Ok(stream);
            }
        }

        let transcoding = download::select(track.transcodings(), opts)?;
        let filename = download::suggested_filename(track, &transcoding.format());
        self.stream_transcoding(transcoding, filename, opts.hls_concurrency)
            .await
    }

    // ---- likes ----------------------------------------------------------

    /// Walk a user's likes, following `next_href` until `limit` entries have been
    /// collected or the endpoint runs dry.
    ///
    /// `limit` of `None` means every page. Liked *playlists* are dropped, matching
    /// the node package — use [`Client::likes_raw`] to keep them.
    pub async fn likes(&self, user_id: u64, limit: Option<u32>, offset: u32) -> Result<Likes> {
        self.walk_likes(user_id, limit, offset, true).await
    }

    /// Like [`Client::likes`], but keeps liked playlists alongside liked tracks.
    pub async fn likes_raw(&self, user_id: u64, limit: Option<u32>, offset: u32) -> Result<Likes> {
        self.walk_likes(user_id, limit, offset, false).await
    }

    /// Resolve a profile url to its user, then walk that user's likes.
    pub async fn likes_for_profile(
        &self,
        profile_url: &str,
        limit: Option<u32>,
        offset: u32,
    ) -> Result<Likes> {
        let user = self.user(profile_url).await?;
        self.likes(user.id, limit, offset).await
    }

    async fn walk_likes(
        &self,
        user_id: u64,
        limit: Option<u32>,
        offset: u32,
        tracks_only: bool,
    ) -> Result<Likes> {
        let mut out = Likes::default();
        let mut remaining = limit;

        let first_page_size = likes::page_size(remaining);
        let mut next: Option<String> = None;

        loop {
            let page: PaginatedQuery<Like> = match &next {
                None => {
                    self.api_get(
                        &format!("/users/{user_id}/likes"),
                        &[
                            ("limit", first_page_size.to_string()),
                            ("offset", offset.to_string()),
                        ],
                    )
                    .await?
                }
                Some(href) => {
                    // The href already carries offset and cursor state; only the
                    // page size needs adjusting as the budget shrinks.
                    let sized =
                        scurl::set_query(href, "limit", &likes::page_size(remaining).to_string())?;
                    self.api_get(&sized, &[]).await?
                }
            };

            out.pages_fetched += 1;
            likes::validate_kinds(&page.collection)?;
            let raw_page_len = page.collection.len();

            let mut kept: Vec<Like> = if tracks_only {
                page.collection.into_iter().filter(Like::is_track).collect()
            } else {
                page.collection
            };

            // Never overshoot an explicit limit.
            if let Some(n) = remaining {
                kept.truncate(n as usize);
                let taken = u32::try_from(kept.len()).unwrap_or(u32::MAX);
                remaining = Some(n.saturating_sub(taken));
            }

            out.collection.append(&mut kept);
            out.next_href = page.next_href.clone();

            if likes::should_stop(remaining, raw_page_len, page.next_href.as_deref()) {
                break;
            }

            if out.pages_fetched >= likes::MAX_PAGES {
                tracing::warn!(
                    user = user_id,
                    pages = out.pages_fetched,
                    "stopping the likes walk at the page cap; next_href is still set"
                );
                break;
            }

            // A cursor that does not advance would otherwise loop forever.
            if next.as_deref() == page.next_href.as_deref() {
                tracing::warn!(
                    user = user_id,
                    "likes cursor stopped advancing; ending the walk"
                );
                break;
            }

            next = page.next_href;
        }

        tracing::debug!(
            user = user_id,
            collected = out.len(),
            pages = out.pages_fetched,
            "walked likes"
        );

        Ok(out)
    }

    // ---- playlists ------------------------------------------------------

    /// Open an audio stream for every track in a playlist.
    ///
    /// The direct analogue of the node package's `downloadPlaylist`. Tracks that
    /// fail keep their error rather than sinking the whole call, so the result is
    /// one entry per track in playlist order.
    ///
    /// Each returned stream holds an open connection until it is consumed or
    /// dropped. For a large playlist, prefer walking [`Client::set`] yourself and
    /// calling [`Client::download_track`] one track at a time.
    pub async fn download_playlist(
        &self,
        url: &str,
        opts: &DownloadOptions,
        concurrency: usize,
    ) -> Result<Vec<(Track, Result<AudioStream>)>> {
        use futures_util::stream::{self, StreamExt};

        let set = self.set(url).await?;

        let opened = stream::iter(set.tracks)
            .map(|track| async move {
                let audio = self.download_track(&track, opts).await;
                (track, audio)
            })
            .buffered(concurrency.clamp(1, 32))
            .collect::<Vec<_>>()
            .await;

        Ok(opened)
    }

    /// Stream audio straight from a transcoding's lookup url, with no `Transcoding`
    /// object in hand.
    ///
    /// The analogue of the node package's `fromURL`. The protocol is inferred from
    /// the url the same way that package infers it — `SoundCloud` spells it out in
    /// the path — so prefer [`Client::stream_transcoding`] whenever the real
    /// transcoding is available.
    pub async fn stream_from_transcoding_url(
        &self,
        transcoding_url: &str,
        filename: impl Into<String>,
    ) -> Result<AudioStream> {
        let protocol = if transcoding_url.contains("/progressive") {
            Protocol::Progressive
        } else {
            Protocol::Hls
        };

        let transcoding = Transcoding {
            url: transcoding_url.to_string(),
            preset: String::new(),
            duration: None,
            snipped: false,
            quality: None,
            format: crate::model::TranscodingFormat {
                protocol,
                // Unknown without the track payload; the caller names the file.
                mime_type: "audio/mpeg".to_string(),
            },
        };

        self.stream_transcoding(&transcoding, filename.into(), None)
            .await
    }

    // ---- search & discovery (thin wrappers over api-v2) ------------------

    /// Raw paginated GET against an api-v2 endpoint path, e.g. `/search/tracks`.
    pub async fn api_get<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        params: &[(&str, String)],
    ) -> Result<T> {
        let base = if path_or_url.starts_with("http") {
            path_or_url.to_string()
        } else {
            format!("{}{}", info::API, path_or_url)
        };

        self.get_json(|cid| {
            let mut pairs: Vec<(&str, &str)> = vec![("client_id", cid)];
            for (k, v) in params {
                pairs.push((k, v.as_str()));
            }
            scurl::append_query(&base, &pairs)
        })
        .await
    }

    /// Follow a `next_href` from a paginated response.
    pub async fn next_page<T: DeserializeOwned>(&self, next_href: &str) -> Result<T> {
        self.api_get(next_href, &[]).await
    }

    /// Search tracks. `resource` is one of `tracks`, `users`, `albums`, `playlists`,
    /// or `all` for the mixed endpoint.
    pub async fn search<T: DeserializeOwned>(
        &self,
        query: &str,
        resource: &str,
        limit: u32,
        offset: u32,
    ) -> Result<PaginatedQuery<T>> {
        let path = if resource == "all" {
            "/search".to_string()
        } else {
            format!("/search/{resource}")
        };

        self.api_get(
            &path,
            &[
                ("q", query.to_string()),
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
            ],
        )
        .await
    }

    /// Tracks related to the given track id.
    pub async fn related(
        &self,
        track_id: u64,
        limit: u32,
        offset: u32,
    ) -> Result<PaginatedQuery<Track>> {
        self.api_get(
            &format!("/tracks/{track_id}/related"),
            &[("limit", limit.to_string()), ("offset", offset.to_string())],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned(ttl_secs: u64, allow_fallback: bool) -> Client {
        Client::builder()
            .client_id("x".repeat(32))
            .client_id_ttl_secs(ttl_secs)
            .allow_scrape_fallback(allow_fallback)
            .cache_client_id("/nonexistent/should-never-be-read.json")
            .build()
            .unwrap()
    }

    /// A ttl of zero would make any *scraped* id instantly stale. A pinned id must
    /// survive it — and must resolve without touching the network or the cache file.
    #[tokio::test]
    async fn a_pinned_id_never_expires() {
        let client = pinned(0, false);
        assert_eq!(client.client_id().await.unwrap(), "x".repeat(32));
        // Still there on a second call, rather than having been re-scraped.
        assert_eq!(client.client_id().await.unwrap(), "x".repeat(32));
    }

    #[tokio::test]
    async fn a_pinned_id_is_not_dropped_on_rejection() {
        let client = pinned(0, false);
        assert!(!client.invalidate_client_id().await);
        assert_eq!(client.client_id().await.unwrap(), "x".repeat(32));
    }

    #[tokio::test]
    async fn the_fallback_opt_in_allows_replacing_a_pinned_id() {
        let client = pinned(0, true);
        assert!(client.invalidate_client_id().await);
        assert!(client.client_id.read().await.is_none());
    }

    #[tokio::test]
    async fn set_client_id_pins_at_runtime() {
        let client = Client::builder().client_id_ttl_secs(0).build().unwrap();
        client.set_client_id("y".repeat(32)).await;

        assert_eq!(client.client_id().await.unwrap(), "y".repeat(32));
        assert!(!client.invalidate_client_id().await);
    }

    #[tokio::test]
    async fn a_scraped_id_is_always_disposable() {
        let client = Client::builder().build().unwrap();
        *client.client_id.write().await =
            Some(IdSource::Scraped(CachedClientId::new("z".repeat(32))));

        assert!(client.invalidate_client_id().await);
        assert!(client.client_id.read().await.is_none());
    }

    #[test]
    fn scraped_ids_expire_but_pinned_ones_do_not() {
        let pinned = IdSource::Pinned("a".repeat(32));
        assert!(pinned.is_usable(0));
        assert!(pinned.is_pinned());

        let mut cached = CachedClientId::new("b".repeat(32));
        cached.fetched_at = 0;
        let scraped = IdSource::Scraped(cached);
        assert!(!scraped.is_usable(client_id::DEFAULT_TTL_SECS));
        assert!(!scraped.is_pinned());
    }
}
