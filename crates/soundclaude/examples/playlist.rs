//! Open a stream for every track in a playlist without downloading them.
//!
//! ```sh
//! cargo run -p soundclaude --example playlist -- <playlist-url>
//! ```

use soundclaude::{Client, DownloadOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .ok_or("usage: playlist <soundcloud playlist url>")?;

    let scdl = Client::new()?;
    let opts = DownloadOptions::new();

    // Each entry keeps its own error, so one dead track does not sink the rest.
    let opened = scdl.download_playlist(&url, &opts, 4).await?;

    for (track, audio) in &opened {
        match audio {
            Ok(stream) => println!(
                "  ok    {:<52} {:<12} {}",
                track.display_name(),
                stream.protocol.as_str(),
                stream.filename
            ),
            Err(err) => println!("  fail  {:<52} {err}", track.display_name()),
        }
    }

    let failed = opened.iter().filter(|(_, a)| a.is_err()).count();
    println!("\n{} track(s), {failed} failed", opened.len());
    Ok(())
}
