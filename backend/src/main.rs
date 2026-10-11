mod config;
mod db;
mod handlers;
mod models;
mod schema;

use crate::config::Config;
use crate::db::DbPool;
use axum::http::StatusCode;
use axum::{routing::get, Router};
use clap::Parser;
use memory_serve::CacheControl;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use ws_bridge::WsEndpoint;

#[derive(Parser, Debug, Clone)]
#[command(name = "backend")]
#[command(about = "Backend server")]
struct Args {
    /// Enable development mode (relaxed config requirements)
    #[arg(long)]
    dev_mode: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub dev_mode: bool,
    pub db_pool: DbPool,
}

/// Build the full application router from shared state.
///
/// Kept as a pure function of `AppState` so tests can drive the entire app
/// in-process via `tower::ServiceExt::oneshot` — no bound port, no network.
pub fn build_app(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Pre-compressed (brotli/gzip), content-negotiated frontend assets with
    // ETag/304 and an SPA fallback to index.html. Assets are embedded at build
    // time in release builds and read from disk in debug builds.
    let frontend = memory_serve::load!()
        .index_file(Some("/index.html"))
        .fallback(Some("/index.html"))
        .fallback_status(StatusCode::OK)
        .html_cache_control(CacheControl::NoCache)
        .cache_control(CacheControl::Long)
        .into_router();

    Router::new()
        .route("/api/health", get(handlers::health::health))
        .with_state(state)
        .route(shared::AppSocket::PATH, handlers::websocket::handler())
        .merge(frontend)
        .layer(cors)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Log panics with their source location and a backtrace via tracing, so
    // crashes are captured in structured logs rather than only on stderr.
    // Set RUST_BACKTRACE=1 to populate the backtrace.
    std::panic::set_hook(Box::new(|panic_info| {
        let backtrace = std::backtrace::Backtrace::capture();
        match panic_info.location() {
            Some(loc) => tracing::error!(
                "PANIC at {}:{}:{}: {}",
                loc.file(),
                loc.line(),
                loc.column(),
                panic_info
            ),
            None => tracing::error!("PANIC: {}", panic_info),
        }
        tracing::error!("Backtrace:\n{backtrace}");
    }));

    if args.dev_mode {
        tracing::warn!("DEV MODE ENABLED");
    }

    // Load .env file if present
    dotenvy::dotenv().ok();

    let config = Config::from_env();

    // Create database pool and run migrations
    let pool = db::create_pool()?;

    tracing::info!("Running database migrations...");
    match db::run_migrations(&pool) {
        Ok(applied) => {
            if applied.is_empty() {
                tracing::info!("Database is up to date");
            } else {
                for m in &applied {
                    tracing::info!("Applied migration: {}", m);
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to run migrations: {}", e);
            return Err(e);
        }
    }

    let app_state = Arc::new(AppState {
        dev_mode: args.dev_mode,
        db_pool: pool,
    });

    let app = build_app(app_state);

    // Bind and serve
    let listener = tokio::net::TcpListener::bind(config.bind_addr()).await?;
    tracing::info!("Listening on {}", listener.local_addr()?);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("Received Ctrl+C, shutting down..."),
        _ = terminate => tracing::info!("Received SIGTERM, shutting down..."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use diesel::pg::PgConnection;
    use diesel::r2d2::{ConnectionManager, Pool};
    use tower::ServiceExt;

    /// State with a pool that is never actually connected. The health and asset
    /// routes don't touch the database, so `build_unchecked` lets us exercise
    /// the whole router without a running Postgres.
    fn test_state() -> Arc<AppState> {
        let manager = ConnectionManager::<PgConnection>::new("postgres://invalid/db");
        let db_pool = Pool::builder().build_unchecked(manager);
        Arc::new(AppState {
            dev_mode: true,
            db_pool,
        })
    }

    #[tokio::test]
    async fn health_returns_ok_json() {
        let resp = build_app(test_state())
            .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("json"), "expected JSON, got {ct}");

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: shared::HealthResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.status, "ok");
    }

    #[tokio::test]
    async fn index_served_as_html() {
        let resp = build_app(test_state())
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("html"), "expected HTML, got {ct}");
    }

    #[tokio::test]
    async fn unknown_path_falls_back_to_index() {
        // SPA fallback: any unmatched path serves index.html with 200.
        let resp = build_app(test_state())
            .oneshot(
                Request::get("/some/client/route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
