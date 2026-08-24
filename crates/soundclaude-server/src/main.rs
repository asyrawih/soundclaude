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
/// `CORS_ALLOW_ORIGIN` to lock it down to one origin in production.
fn cors_layer() -> CorsLayer {
    use axum::http::HeaderValue;

    match std::env::var("CORS_ALLOW_ORIGIN") {
        Ok(origin) => {
            if let Ok(value) = origin.parse::<HeaderValue>() {
                CorsLayer::new()
                    .allow_origin(value)
                    .allow_methods([axum::http::Method::GET])
            } else {
                tracing::warn!(%origin, "CORS_ALLOW_ORIGIN is not a valid header value; allowing any origin");
                CorsLayer::permissive()
            }
        }
        Err(_) => CorsLayer::permissive(),
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
