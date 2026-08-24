# soundclaude

A Rust rewrite of [`node-soundcloud-downloader`](https://github.com/zackradisic/node-soundcloud-downloader) — SoundCloud metadata and audio, with no Node runtime and no ffmpeg.

The workspace has three crates:

| crate | what it is |
| --- | --- |
| `soundclaude` | the library: client_id scraping, api-v2 access, progressive + HLS downloading |
| `soundclaude-cli` | the `scdl` binary |
| `soundclaude-server` | an HTTP backend that serves metadata as JSON and audio as a stream |

## Build

```sh
cargo build --release
```

Binaries land in `target/release/{scdl,soundclaude-server}`.

## CLI

```sh
scdl info    <url> [--json]
scdl get     <url> [-o FILE] [--format mp3|opus|aac] [--protocol progressive|hls] [--no-direct] [-c N]
scdl playlist <url> [-o DIR] [-j JOBS] [--number]
scdl search  <query> [-k tracks|users|albums|playlists|all] [-l LIMIT] [--json]
scdl likes   <profile-url|user-id> [-l LIMIT] [--all] [--playlists] [--json]
scdl user    <profile-url> [--json]
scdl related <track-id> [-l LIMIT] [--json]
scdl client-id [--check]
```

```sh
scdl info https://soundcloud.com/artist/track
scdl get  https://soundcloud.com/artist/track -o song.mp3
scdl playlist https://soundcloud.com/artist/sets/album -o ./album -j 4 --number
scdl likes https://soundcloud.com/artist --limit 100
scdl likes 173476 --all --playlists
```

Mobile links (`m.soundcloud.com/...`) and app share links (`soundcloud.app.goo.gl/...`) are normalized automatically.

The scraped `client_id` is cached for a day under the platform cache dir; `--no-cache`
disables that, `--client-id` / `SOUNDCLOUD_CLIENT_ID` supplies your own.

An id you supply is **pinned**: it never expires, is never read from or written to the
cache, and is never silently replaced by a scraped one. If SoundCloud rejects it you get
an error saying so, rather than a working command that quietly ignored your flag. Pass
`--allow-scrape-fallback` to prefer staying up over being told.

## Library

```rust
use soundclaude::{Client, DownloadOptions, Format};

let scdl = Client::new()?;

let track = scdl.track("https://soundcloud.com/artist/track").await?;
println!("{} — {:?} ms", track.display_name(), track.duration);

// Default selection prefers progressive mp3, and tries the artist's original
// file first when the track offers one.
scdl.download("https://soundcloud.com/artist/track")
    .await?
    .save("out.mp3")
    .await?;

// Or pin the rendition explicitly.
let opts = DownloadOptions::new().format(Format::Aac).use_download_link(false);
let audio = scdl.download_with("https://soundcloud.com/artist/track", &opts).await?;
println!("{} ({})", audio.filename, audio.mime_type);
```

`AudioStream` implements `Stream<Item = Result<Bytes>>`, so it can be piped anywhere
rather than buffered: `save`, `write_to`, `bytes`, or `into_stream`.

Playlists resolve with their track stubs filled in:

```rust
let set = scdl.set("https://soundcloud.com/artist/sets/album").await?;
for track in &set.tracks {
    println!("{}", track.display_name());
}
```

If you obtained a `Set` from `resolve()` instead, call `hydrate_set(&mut set)` —
`/resolve` returns full objects for only the first few tracks and bare ids for the rest.

Likes are paginated, and the walk is handled for you:

```rust
// 100 liked tracks, following next_href across as many pages as that takes.
let likes = scdl.likes(user_id, Some(100), 0).await?;
for track in likes.tracks() {
    println!("{}", track.display_name());
}

// Every page. Liked playlists are dropped unless you ask for likes_raw.
let all = scdl.likes_for_profile("https://soundcloud.com/artist", None, 0).await?;
```

`limit` counts *kept* entries, not raw ones: with the default tracks-only filter, a
page made largely of liked playlists contributes little and the walk continues. It is
bounded by `likes::MAX_PAGES`, and stops early if the cursor ever fails to advance.

## Server

```sh
PORT=8080 soundclaude-server
```

| route | returns |
| --- | --- |
| `GET /health` | liveness |
| `GET /v1/resolve?url=` | the raw `/resolve` payload |
| `GET /v1/track?url=` | track metadata |
| `GET /v1/set?url=` | playlist metadata, tracks hydrated |
| `GET /v1/user?url=` | user profile |
| `GET /v1/likes?id=\|url=&limit=&offset=&playlists=` | a user's likes, pages walked server-side |
| `GET /v1/search?q=&kind=&limit=&offset=` | search results |
| `GET /v1/related?id=&limit=&offset=` | related tracks |
| `GET /v1/stream?url=&format=&protocol=&no_direct=` | audio, `Content-Disposition: inline` |
| `GET /v1/download?url=&…` | audio, `Content-Disposition: attachment` |

Audio is streamed as it arrives — progressive downloads carry a `Content-Length`,
HLS is chunked because the total size isn't known until the last segment lands.
Errors come back as `{"error":{"kind":…,"message":…}}`; a stale `client_id` on our
side is a `502`, not a `4xx`.

Env: `BIND` or `PORT`, `SOUNDCLOUD_CLIENT_ID`, `ALLOW_SCRAPE_FALLBACK`, `SOUNDCLAUDE_CACHE`,
`CORS_ALLOW_ORIGIN` (defaults to permissive), `RUST_LOG`.

A `SOUNDCLOUD_CLIENT_ID` that SoundCloud rejects surfaces as a `500` with kind
`client_id_rejected` — this deployment is misconfigured, which is neither the caller's
fault (`4xx`) nor upstream being down (`502`). Set `ALLOW_SCRAPE_FALLBACK=1` to scrape a
replacement and keep serving instead.

## How it works

SoundCloud has no public key issuance any more, so the library scrapes the `client_id`
out of the web player's own JS bundles, exactly as the browser uses it, and caches it for
a day. A `401` mid-request means the scraped id rotated — every API call retries once with
a fresh one.

Scraped and supplied ids are deliberately not interchangeable. A scraped id is disposable:
it expires, it round-trips through the cache file, and it is replaced on rejection without
comment. A supplied id is configuration, so none of that happens to it. `verify_client_id`
checks an id against the API without making a real request — the equivalent of
`soundcloud-key-fetch`'s `keyIsValid`.

A track exposes several *transcodings*, each with a protocol:

- **progressive** — one HTTP response holding the whole file.
- **hls** — an m3u8 playlist. The segments are fetched with bounded look-ahead
  concurrency and emitted strictly in playlist order, so concatenating them
  reproduces the file byte for byte. fMP4 presets (`EXT-X-MAP`) are handled by
  putting the init segment first. No ffmpeg is involved.

Encrypted playlists (`EXT-X-KEY` with a method other than `NONE`) are rejected rather
than silently producing noise.

Watch out for `audio/mpegurl`: it is the `abr_sq` adaptive-bitrate playlist, *not*
mpeg audio, despite the shared mime prefix.

## Tests

```sh
cargo test --workspace
```

The suite covers the pure logic — url classification, m3u8 parsing, transcoding
selection, playlist hydration order, client_id pinning, batching, filename and header
escaping — and does not touch the network.

```sh
cargo clippy --workspace --all-targets   # clean, with pedantic on
cargo fmt --all --check
```

`clippy::pedantic` is enabled workspace-wide in the root `Cargo.toml`. Four lints are
switched off there, each with the reason inline — they are publishing conventions or
readability regressions, not findings worth chasing.

Unlike the library, `/v1/likes` always caps `limit` (at 500) — one request there
should not turn into an unbounded number of upstream ones.

## Parity with the Node package

Everything is ported: `download`, `downloadFormat`, `getInfo`, `getTrackInfoByID`,
`getSetInfo`, `downloadPlaylist`, `search`, `related`, `getLikes`, `getUser`,
`filterMedia`, the url helpers, and `soundcloud-key-fetch`'s `fetchKey` / `keyIsValid`.

Two bugs in the original are *not* reproduced: `fromMediaObj` tested the truthiness of
its `validatemedia` function instead of calling it, and `isURL`'s mobile branch indexed
the match of the wrong regex.

For endpoints with no wrapper, `Client::api_get` and `Client::next_page` reach any
api-v2 path directly.

## Legal

Downloading is governed by SoundCloud's terms of service and by the rights of whoever
made the track. This is a transport, not a licence.

MIT, matching the project it is a port of.
