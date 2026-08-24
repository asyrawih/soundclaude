//! `scdl` — download audio and metadata from `SoundCloud`.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use soundclaude::{Client, DownloadOptions, Format, Protocol, Track};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// Doc comments here are clap help text shown in a terminal, where backticks are
// literal characters rather than markup.
#[allow(clippy::doc_markdown)]
#[derive(Parser, Debug)]
#[command(
    name = "scdl",
    version,
    about = "Download SoundCloud audio and metadata",
    long_about = None,
)]
struct Cli {
    /// Use this client_id instead of scraping one from soundcloud.com.
    #[arg(long, global = true, env = "SOUNDCLOUD_CLIENT_ID")]
    client_id: Option<String>,

    /// Where to cache the scraped client_id (defaults to the user cache dir).
    #[arg(long, global = true, env = "SOUNDCLAUDE_CACHE")]
    cache: Option<PathBuf>,

    /// Do not read or write the client_id cache.
    #[arg(long, global = true)]
    no_cache: bool,

    /// If the client_id you supplied is rejected, scrape a replacement instead of
    /// failing. Only meaningful together with --client-id.
    #[arg(long, global = true)]
    allow_scrape_fallback: bool,

    /// Log more (repeat for debug/trace level).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[allow(clippy::doc_markdown)]
#[derive(Subcommand, Debug)]
enum Command {
    /// Print metadata for a track, playlist, or user.
    Info {
        /// A soundcloud.com, m.soundcloud.com, or soundcloud.app.goo.gl url.
        url: String,
        /// Print the raw API payload instead of a summary.
        #[arg(long)]
        json: bool,
    },

    /// Download a single track.
    Get {
        url: String,
        /// Output file. Defaults to "<Artist> - <Title>.<ext>" in the current directory.
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        media: MediaArgs,
    },

    /// Download every track in a playlist or album.
    Playlist {
        url: String,
        /// Directory to write into (created if missing).
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
        /// How many tracks to download at once.
        #[arg(short = 'j', long, default_value_t = 3)]
        jobs: usize,
        /// Prefix each file with its playlist position.
        #[arg(long)]
        number: bool,
        #[command(flatten)]
        media: MediaArgs,
    },

    /// Search SoundCloud.
    Search {
        query: Vec<String>,
        /// One of: tracks, users, albums, playlists, all.
        #[arg(short, long, default_value = "tracks")]
        kind: String,
        #[arg(short, long, default_value_t = 10)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },

    /// List the tracks a user has liked.
    Likes {
        /// A profile url, or a bare numeric user id.
        who: String,
        /// How many likes to collect.
        #[arg(short, long, default_value_t = 50)]
        limit: u32,
        /// Collect every page instead of stopping at --limit.
        #[arg(long)]
        all: bool,
        /// Include liked playlists, not just liked tracks.
        #[arg(long)]
        playlists: bool,
        #[arg(long)]
        json: bool,
    },

    /// Print a user profile.
    User {
        /// The profile url.
        url: String,
        #[arg(long)]
        json: bool,
    },

    /// List tracks related to a track.
    Related {
        /// Numeric track id (see scdl info).
        id: u64,
        #[arg(short, long, default_value_t = 10)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },

    /// Print a usable client_id (scraping one if needed).
    ClientId {
        /// Verify the id against the API before printing it.
        #[arg(long)]
        check: bool,
    },
}

#[allow(clippy::doc_markdown)]
#[derive(Args, Debug, Clone)]
struct MediaArgs {
    /// Audio format: mp3, opus, or aac.
    #[arg(short, long)]
    format: Option<String>,

    /// Delivery protocol: progressive or hls.
    #[arg(short, long)]
    protocol: Option<String>,

    /// Skip the artist's original-file download even when it is offered.
    #[arg(long)]
    no_direct: bool,

    /// HLS segments to fetch concurrently.
    #[arg(short = 'c', long, default_value_t = 6)]
    concurrency: usize,
}

impl MediaArgs {
    fn to_options(&self) -> Result<DownloadOptions> {
        let mut opts = DownloadOptions::new()
            .use_download_link(!self.no_direct)
            .hls_concurrency(self.concurrency);

        if let Some(f) = &self.format {
            opts = opts.format(f.parse::<Format>().map_err(anyhow::Error::msg)?);
        }
        if let Some(p) = &self.protocol {
            let protocol: Protocol = p.parse().expect("infallible");
            if let Protocol::Other(other) = &protocol {
                bail!("unknown protocol `{other}` (expected `progressive` or `hls`)");
            }
            opts = opts.protocol(protocol);
        }
        Ok(opts)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let client = build_client(&cli)?;

    match cli.command {
        Command::Info { url, json } => info(&client, &url, json).await,
        Command::Get { url, output, media } => {
            get(&client, &url, output.as_deref(), &media.to_options()?).await
        }
        Command::Playlist {
            url,
            output,
            jobs,
            number,
            media,
        } => playlist(&client, &url, &output, jobs, number, &media.to_options()?).await,
        Command::Search {
            query,
            kind,
            limit,
            json,
        } => search(&client, &query.join(" "), &kind, limit, json).await,
        Command::Likes {
            who,
            limit,
            all,
            playlists,
            json,
        } => likes(&client, &who, (!all).then_some(limit), playlists, json).await,
        Command::User { url, json } => user(&client, &url, json).await,
        Command::Related { id, limit, json } => related(&client, id, limit, json).await,
        Command::ClientId { check } => {
            let id = client.client_id().await?;
            if check && !client.verify_client_id(&id).await? {
                bail!("client_id {id} was rejected by soundcloud");
            }
            println!("{id}");
            Ok(())
        }
    }
}

fn init_tracing(verbosity: u8) {
    let default = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "soundclaude=debug,scdl=debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default.into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn build_client(cli: &Cli) -> Result<Client> {
    let mut builder = Client::builder();

    if let Some(id) = &cli.client_id {
        builder = builder.client_id(id.clone());
    }
    if cli.allow_scrape_fallback {
        builder = builder.allow_scrape_fallback(true);
    }
    if !cli.no_cache {
        let path = cli.cache.clone().unwrap_or_else(default_cache_path);
        builder = builder.cache_client_id(path);
    }

    builder.build().context("could not build the http client")
}

fn default_cache_path() -> PathBuf {
    // No dirs crate: HOME plus the platform's usual cache location is enough.
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let home = PathBuf::from(home);
                if cfg!(target_os = "macos") {
                    home.join("Library/Caches")
                } else {
                    home.join(".cache")
                }
            })
        })
        .unwrap_or_else(std::env::temp_dir);

    base.join("soundclaude").join("client_id.json")
}

// ---- commands -----------------------------------------------------------

async fn info(client: &Client, url: &str, json: bool) -> Result<()> {
    use soundclaude::Resolved;

    let mut resolved = client.resolve(url).await?;

    // /resolve leaves a playlist's tracks as bare ids past the first few.
    if let Resolved::Playlist(set) = &mut resolved {
        client.hydrate_set(set).await?;
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&resolved)?);
        return Ok(());
    }

    match resolved {
        Resolved::Track(track) => print_track(&track),
        Resolved::Playlist(set) => {
            println!("{}", set.title.as_deref().unwrap_or("(untitled playlist)"));
            if let Some(user) = &set.user {
                println!("  by         {}", user.username);
            }
            println!("  tracks     {}", set.tracks.len());
            if let Some(ms) = set.duration {
                println!("  duration   {}", human_duration(ms));
            }
            println!(
                "  url        {}",
                set.permalink_url.as_deref().unwrap_or("-")
            );
            println!();
            for (i, track) in set.tracks.iter().enumerate() {
                println!(
                    "  {:>3}. {:<60} {}",
                    i + 1,
                    truncate(&track.display_name(), 60),
                    track.duration.map(human_duration).unwrap_or_default()
                );
            }
        }
        Resolved::User(user) => {
            println!("{}", user.username);
            println!("  followers  {}", user.followers_count.unwrap_or(0));
            println!("  tracks     {}", user.track_count.unwrap_or(0));
            println!(
                "  url        {}",
                user.permalink_url.as_deref().unwrap_or("-")
            );
        }
        Resolved::Unknown => bail!("soundcloud resolved this url to an unsupported resource kind"),
    }
    Ok(())
}

fn print_track(track: &Track) {
    println!("{}", track.display_name());
    println!("  id         {}", track.id);
    if let Some(ms) = track.duration {
        println!("  duration   {}", human_duration(ms));
    }
    if let Some(genre) = track.genre.as_deref().filter(|g| !g.is_empty()) {
        println!("  genre      {genre}");
    }
    println!("  plays      {}", track.playback_count.unwrap_or(0));
    println!("  likes      {}", track.likes_count.unwrap_or(0));
    println!(
        "  original   {}",
        if track.downloadable && track.has_downloads_left {
            "available"
        } else {
            "not offered"
        }
    );
    println!(
        "  url        {}",
        track.permalink_url.as_deref().unwrap_or("-")
    );

    let transcodings = track.transcodings();
    if !transcodings.is_empty() {
        println!("  media");
        for t in transcodings {
            println!(
                "    {:<12} {:<28} {}",
                t.protocol().as_str(),
                t.format.mime_type,
                t.preset
            );
        }
    }
}

async fn get(
    client: &Client,
    url: &str,
    output: Option<&Path>,
    opts: &DownloadOptions,
) -> Result<()> {
    let track = client.track(url).await?;
    eprintln!("{}", track.display_name());

    let audio = client.download_track(&track, opts).await?;
    let path = output.map_or_else(|| PathBuf::from(&audio.filename), Path::to_path_buf);

    let bar = progress_bar(audio.content_length, &audio.filename);
    if audio.content_length.is_none() {
        bar.set_message(format!("{} (hls)", audio.filename));
    }

    let written = save(audio, &path, &bar).await?;
    bar.finish_and_clear();

    eprintln!("saved {} ({})", path.display(), human_bytes(written));
    Ok(())
}

async fn playlist(
    client: &Client,
    url: &str,
    dir: &Path,
    jobs: usize,
    number: bool,
    opts: &DownloadOptions,
) -> Result<()> {
    use futures_util::stream::StreamExt;

    let set = client.set(url).await?;
    let total = set.tracks.len();
    eprintln!(
        "{} — {total} track(s)",
        set.title.as_deref().unwrap_or("(untitled playlist)")
    );
    tokio::fs::create_dir_all(dir).await?;

    let multi = Arc::new(MultiProgress::new());
    let width = total.to_string().len();

    let results: Vec<(String, Result<PathBuf>)> =
        futures_util::stream::iter(set.tracks.into_iter().enumerate())
            .map(|(idx, track)| {
                let client = client.clone();
                let opts = opts.clone();
                let dir = dir.to_path_buf();
                let multi = Arc::clone(&multi);

                async move {
                    let name = track.display_name();
                    let outcome = async {
                        let audio = client.download_track(&track, &opts).await?;
                        let filename = if number {
                            format!("{:0width$} - {}", idx + 1, audio.filename, width = width)
                        } else {
                            audio.filename.clone()
                        };
                        let path = dir.join(&filename);

                        let bar = multi.add(progress_bar(audio.content_length, &filename));
                        let written = save(audio, &path, &bar).await;
                        bar.finish_and_clear();
                        written?;
                        Ok::<_, anyhow::Error>(path)
                    }
                    .await;

                    (name, outcome)
                }
            })
            .buffer_unordered(jobs.max(1))
            .collect()
            .await;

    let failed: Vec<_> = results
        .iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| (name, e)))
        .collect();

    eprintln!(
        "\ndownloaded {}/{} track(s) into {}",
        total - failed.len(),
        total,
        dir.display()
    );
    for (name, err) in &failed {
        eprintln!("  failed: {name}: {err}");
    }

    if failed.len() == total && total > 0 {
        bail!("every track failed");
    }
    Ok(())
}

async fn search(client: &Client, query: &str, kind: &str, limit: u32, json: bool) -> Result<()> {
    if query.trim().is_empty() {
        bail!("a search query is required");
    }

    let page: soundclaude::PaginatedQuery<serde_json::Value> =
        client.search(query, kind, limit, 0).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    for item in &page.collection {
        let title = item
            .get("title")
            .or_else(|| item.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("(untitled)");
        let by = item
            .get("user")
            .and_then(|u| u.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let link = item
            .get("permalink_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if by.is_empty() {
            println!("{:<50}  {link}", truncate(title, 50));
        } else {
            println!(
                "{:<50}  {:<20}  {link}",
                truncate(title, 50),
                truncate(by, 20)
            );
        }
    }
    Ok(())
}

async fn likes(
    client: &Client,
    who: &str,
    limit: Option<u32>,
    include_playlists: bool,
    json: bool,
) -> Result<()> {
    // A bare number is a user id; anything else has to be resolved as a profile.
    let user_id = match who.trim().parse::<u64>() {
        Ok(id) => id,
        Err(_) => client.user(who).await?.id,
    };

    let likes = if include_playlists {
        client.likes_raw(user_id, limit, 0).await?
    } else {
        client.likes(user_id, limit, 0).await?
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&likes)?);
        return Ok(());
    }

    for like in &likes.collection {
        match (&like.track, &like.playlist) {
            (Some(track), _) => println!(
                "{:<8} {:<58} {}",
                track.id,
                truncate(&track.display_name(), 58),
                track.permalink_url.as_deref().unwrap_or("-")
            ),
            (None, Some(set)) => println!(
                "{:<8} {:<58} {}",
                set.id,
                truncate(
                    &format!(
                        "[playlist] {}",
                        set.title.as_deref().unwrap_or("(untitled)")
                    ),
                    58
                ),
                set.permalink_url.as_deref().unwrap_or("-")
            ),
            (None, None) => {}
        }
    }

    eprintln!(
        "\n{} like(s) over {} page(s){}",
        likes.len(),
        likes.pages_fetched,
        if likes.next_href.is_some() {
            ", more available"
        } else {
            ""
        }
    );
    Ok(())
}

async fn user(client: &Client, url: &str, json: bool) -> Result<()> {
    let user = client.user(url).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&user)?);
        return Ok(());
    }

    println!("{}", user.username);
    println!("  id         {}", user.id);
    if let Some(name) = user.full_name.as_deref().filter(|n| !n.is_empty()) {
        println!("  full name  {name}");
    }
    if let Some(city) = user.city.as_deref().filter(|c| !c.is_empty()) {
        println!("  city       {city}");
    }
    println!("  followers  {}", user.followers_count.unwrap_or(0));
    println!("  following  {}", user.followings_count.unwrap_or(0));
    println!("  tracks     {}", user.track_count.unwrap_or(0));
    println!("  verified   {}", user.verified.unwrap_or(false));
    println!(
        "  url        {}",
        user.permalink_url.as_deref().unwrap_or("-")
    );
    Ok(())
}

async fn related(client: &Client, id: u64, limit: u32, json: bool) -> Result<()> {
    let page = client.related(id, limit, 0).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    for track in &page.collection {
        println!(
            "{:<8} {:<58} {}",
            track.id,
            truncate(&track.display_name(), 58),
            track.permalink_url.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

// ---- helpers ------------------------------------------------------------

async fn save(audio: soundclaude::AudioStream, path: &Path, bar: &ProgressBar) -> Result<u64> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let mut file = tokio::fs::File::create(path)
        .await
        .with_context(|| format!("could not create {}", path.display()))?;

    let written = audio
        .write_to(&mut file, |so_far| bar.set_position(so_far))
        .await?;

    Ok(written)
}

fn progress_bar(total: Option<u64>, label: &str) -> ProgressBar {
    let bar = if let Some(len) = total {
        let bar = ProgressBar::new(len);
        bar.set_style(
            ProgressStyle::with_template(
                "{msg:<40!} [{bar:28}] {bytes:>10}/{total_bytes:<10} {bytes_per_sec}",
            )
            .expect("progress template")
            .progress_chars("=> "),
        );
        bar
    } else {
        // HLS has no known total until the last segment lands.
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner} {msg:<40!} {bytes:>10} {bytes_per_sec}")
                .expect("spinner template"),
        );
        bar.enable_steady_tick(Duration::from_millis(120));
        bar
    };
    bar.set_message(label.to_string());
    bar
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn human_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    // Display only: f64 loses precision past 2^53 bytes, which is 8 petabytes.
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_and_sizes() {
        assert_eq!(human_duration(0), "0:00");
        assert_eq!(human_duration(65_000), "1:05");
        assert_eq!(human_duration(3_725_000), "1:02:05");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("ééééé", 3), "éé…");
    }

    #[test]
    fn media_args_reject_unknown_values() {
        let bad_format = MediaArgs {
            format: Some("flac".into()),
            protocol: None,
            no_direct: false,
            concurrency: 6,
        };
        assert!(bad_format.to_options().is_err());

        let bad_protocol = MediaArgs {
            format: None,
            protocol: Some("dash".into()),
            no_direct: false,
            concurrency: 6,
        };
        assert!(bad_protocol.to_options().is_err());

        let good = MediaArgs {
            format: Some("mp3".into()),
            protocol: Some("hls".into()),
            no_direct: true,
            concurrency: 4,
        };
        let opts = good.to_options().unwrap();
        assert_eq!(opts.format, Some(Format::Mp3));
        assert_eq!(opts.protocol, Some(Protocol::Hls));
        assert!(!opts.use_download_link);
    }

    #[test]
    fn cli_parses_the_documented_invocations() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
