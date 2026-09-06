//! `soundclaude-server` — an HTTP front end for the `soundclaude` library.

mod error;
mod routes;

use anyhow::{Context, Result};
use routes::AppState;
use soundclaude::Client;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "soundclaude_server=info,soundclaude=info,tower_http=info".into()
            }),
        )
        .init();

    let addr: SocketAddr = std::env::var("BIND")
        .unwrap_or_else(|_| {
            let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
            format!("0.0.0.0:{port}")
        })
        .parse()
        .context("BIND must be host:port")?;

    let mut builder = Client::builder();
    if let Ok(id) = std::env::var("SOUNDCLOUD_CLIENT_ID") {
        builder = builder.client_id(id);

        // A supplied id is pinned: if soundcloud rejects it, requests fail rather
        // than quietly switching to a scraped one. A long-running service can opt
        // into staying up instead.
        if env_flag("ALLOW_SCRAPE_FALLBACK") {
            tracing::info!("scrape fallback enabled for the supplied client_id");
            builder = builder.allow_scrape_fallback(true);
        }
    }
    if let Ok(path) = std::env::var("SOUNDCLAUDE_CACHE") {
        builder = builder.cache_client_id(PathBuf::from(path));
    }
    let scdl = builder.build().context("could not build the http client")?;

    // Fetch a client_id up front so the first real request isn't paying for it —
    // and so a broken deploy fails loudly at boot rather than on request #1.
    match scdl.client_id().await {
        Ok(_) => tracing::info!("client_id ready"),
        Err(err) => tracing::warn!(%err, "starting without a client_id; will retry per request"),
    }

    let state = AppState {
        scdl: Arc::new(scdl),
    };

    let app = routes::router(state)
        .layer(TraceLayer::new_for_http())
        // Generous: a long HLS track legitimately takes a while to concatenate.
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            Duration::from_secs(600),
        ))
        .layer(cors_layer());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("could not bind {addr}"))?;

    tracing::info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Permissive by default so a local web player can call this; set
/// `CORS_ALLOW_ORIGIN` to lock it down in production.
///
/// The value is a comma-separated list, not a single origin: the desktop app
/// (Tauri) calls this server from `tauri://localhost` on macOS and
/// `http://tauri.localhost` on Windows, alongside the web origin. A single
/// origin here silently broke the desktop build — every request passed the
/// server and died in the `WebView`'s CORS check with no server-side log.
///
///     CORS_ALLOW_ORIGIN=https://studio.kelasmalam.app,tauri://localhost,http://tauri.localhost
fn cors_layer() -> CorsLayer {
    let Ok(raw) = std::env::var("CORS_ALLOW_ORIGIN") else {
        return CorsLayer::permissive();
    };
    if let Some(origins) = parse_allowed_origins(&raw) {
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([axum::http::Method::GET])
            .allow_headers([axum::http::header::RANGE])
    } else {
        tracing::warn!(%raw, "CORS_ALLOW_ORIGIN has no valid origin; allowing any origin");
        CorsLayer::permissive()
    }
}

/// Split `CORS_ALLOW_ORIGIN` on commas, trim, drop empties and anything that
/// is not a valid header value. `None` when nothing usable remains — the
/// caller then falls back to permissive with a warning rather than locking
/// everyone out because of a typo.
fn parse_allowed_origins(raw: &str) -> Option<Vec<axum::http::HeaderValue>> {
    let origins: Vec<axum::http::HeaderValue> = raw
        .split(',')
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .filter_map(|o| {
            if let Ok(v) = o.parse::<axum::http::HeaderValue>() {
                Some(v)
            } else {
                tracing::warn!(origin = %o, "CORS_ALLOW_ORIGIN entry is not a valid header value; skipped");
                None
            }
        })
        .collect();
    if origins.is_empty() {
        None
    } else {
        Some(origins)
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("could not install the ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("could not install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("shutting down");
}

#[cfg(test)]
mod cors_tests {
    use super::parse_allowed_origins;

    #[test]
    fn single_origin_still_works() {
        let v = parse_allowed_origins("https://studio.kelasmalam.app").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], "https://studio.kelasmalam.app");
    }

    #[test]
    fn list_with_desktop_origins_and_spaces() {
        let v = parse_allowed_origins(
            " https://studio.kelasmalam.app, tauri://localhost ,http://tauri.localhost,, ",
        )
        .unwrap();
        let s: Vec<&str> = v.iter().map(|h| h.to_str().unwrap()).collect();
        assert_eq!(
            s,
            [
                "https://studio.kelasmalam.app",
                "tauri://localhost",
                "http://tauri.localhost"
            ]
        );
    }

    #[test]
    fn invalid_entries_are_skipped_and_empty_list_is_none() {
        assert!(parse_allowed_origins("").is_none());
        assert!(parse_allowed_origins(" , ,").is_none());
        let v = parse_allowed_origins("https://ok.example,\u{1}bad").unwrap();
        assert_eq!(v.len(), 1);
    }
}
