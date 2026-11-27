mod apps;
mod auth;
mod backups;
mod blueprints;
mod deployments;
mod maintenance;
mod nodes;
mod notifications;
mod projects;
mod settings;
mod webhooks;
mod ws;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;

use crate::auth::crypto::Cipher;
use crate::auth::throttle::LoginThrottle;
use crate::deploy::{DeployTiming, KeyedLocks};
use crate::error::AppError;
use crate::executor::NodeRegistry;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub cipher: Cipher,
    /// One-time setup token, present only while no user exists. The async mutex also
    /// serializes concurrent setup requests so only one admin can ever be created.
    pub setup_token: Arc<Mutex<Option<String>>>,
    pub registry: Arc<NodeRegistry>,
    pub data_dir: PathBuf,
    pub deploy_locks: Arc<KeyedLocks>,
    pub proxy_locks: Arc<KeyedLocks>,
    /// Per app: serializes changes to its containers' run state (panel start/stop/restart
    /// and self-healing), so neither acts on a state the other just changed.
    pub container_locks: Arc<KeyedLocks>,
    pub timing: DeployTiming,
    pub login_throttle: Arc<LoginThrottle>,
}

impl AppState {
    pub fn new(
        db: SqlitePool,
        cipher: Cipher,
        setup_token: Option<String>,
        registry: NodeRegistry,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            db,
            cipher,
            setup_token: Arc::new(Mutex::new(setup_token)),
            registry: Arc::new(registry),
            data_dir,
            deploy_locks: Arc::default(),
            proxy_locks: Arc::default(),
            container_locks: Arc::default(),
            timing: DeployTiming::default(),
            login_throttle: Arc::default(),
        }
    }
}

/// Marks requests that arrived on the listener Caddy proxies to (served over HTTPS).
#[derive(Debug, Clone, Copy)]
pub struct ViaProxy;

/// The browser's address: the TCP peer, or for proxied requests the address Caddy
/// reports in `X-Forwarded-For` (Caddy discards any value the client sent itself).
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub std::net::IpAddr);

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .nest("/auth", auth::routes())
        .nest("/nodes", nodes::routes())
        .nest("/projects", projects::routes())
        .nest("/apps", apps::routes())
        .nest("/deployments", deployments::routes())
        .nest("/settings", settings::routes())
        .nest("/maintenance", maintenance::routes())
        .nest("/notifications", notifications::routes())
        .nest("/blueprints", blueprints::routes())
        .nest("/s3-targets", backups::s3_routes())
        .nest("/backups", backups::backup_routes())
        .fallback(|| async { AppError::NotFound });

    Router::new()
        .nest("/api/v1", api)
        .nest("/webhooks", webhooks::routes())
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .fallback(static_asset)
        .layer(middleware::from_fn(require_json))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// State-changing API requests must be `application/json`. Combined with SameSite=Lax
/// cookies this forces a CORS preflight for any cross-site request (CSRF defence).
async fn require_json(req: Request, next: Next) -> Response {
    let state_changing = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if state_changing && req.uri().path().starts_with("/api/") {
        let is_json = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
            });
        if !is_json {
            return AppError::UnsupportedMediaType.into_response();
        }
    }
    next.run(req).await
}

#[derive(RustEmbed)]
#[folder = "web/dist"]
struct Assets;

/// Serve the embedded SPA; unknown paths fall back to `index.html` for client routing.
async fn static_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (file, name) = match Assets::get(path).filter(|_| !path.is_empty()) {
        Some(file) => (file, path),
        None => match Assets::get("index.html") {
            Some(file) => (file, "index.html"),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let cache = if name.starts_with("_app/immutable/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = Response::new(Body::from(file.data.into_owned()));
    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime.as_ref()) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}
