use crate::auth::EmailCheck;
use crate::error::{ProxyError, UpstreamFetchError};
use crate::playlist::{load_playlist, serialize_guest_playlist, serialize_playlist};
use crate::proxy::{attach_to_stream, prewarm as prewarm_upstream};
use crate::state::AppState;
use crate::sync::{sync_delete, sync_sse, sync_update};
use axum::http::{HeaderMap, Response};
use axum::middleware;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use bytes::Bytes;
use serde::Serialize;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;

#[derive(Serialize)]
struct PrewarmResponse {
    success: bool,
    message: String,
}

pub async fn serve() {
    let app_state = load_playlist().await;
    let app_state_clone = app_state.clone();
    let auth_layer = EmailCheck::new(
        app_state_clone.config.admin_email,
        app_state_clone.config.whitelisted_emails,
        app_state_clone.config.env,
    );
    let guest_auth_layer = auth_layer.clone();

    let cors = CorsLayer::permissive(); // TODO(caio): make cors configurable - only allow origin from config

    // TODO (caio): add tracing layer middleware
    let app = Router::new()
        .route("/", get(load))
        .route("/channel/{title}", get(proxy))
        .route("/prewarm/{title}", post(prewarm_channel))
        .route_layer(middleware::from_fn(move |req, next| {
            let check = auth_layer.clone();
            check.admin_middleware(req, next)
        }))
        .route("/guest", get(guest))
        .route("/guest/playlist", get(guest_playlist))
        .route_layer(middleware::from_fn(move |req, next| {
            let check = guest_auth_layer.clone();
            check.guest_middleware(req, next)
        }))
        // Sync routes - SSE hub for multi-client synchronization
        .route("/sync", get(sync_sse))
        .route("/sync", post(sync_update))
        .route("/sync", delete(sync_delete))
        .with_state(app_state)
        .layer(cors);

    let listener =
        tokio::net::TcpListener::bind(format!("0.0.0.0:{}", app_state_clone.config.port))
            .await
            .unwrap();
    println!("Starting to listen in port {}", app_state_clone.config.port);
    axum::serve(listener, app).await.unwrap();
}

/*
 * We need this for the proxy to be transparent and "backwards compatible"
 * i.e.: user can set the proxy url as if it was talking with the upstream
 */
async fn load(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Response<Body>, UpstreamFetchError> {
    // Extract Host header (falls back to localhost:port if absent)
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("localhost:{}", state.config.port));

    let body = serialize_playlist(&state, &host);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/octet-stream")
        .header("Cache-Control", "no-cache")
        .body(Body::from(body.unwrap()))
        .map_err(|e| {
            tracing::error!("failed to build response: {}", e);
            UpstreamFetchError::ResponseBuild { source: e }
        })?)
}

async fn proxy(
    Path(title): Path<String>,
    State(state): State<AppState>,
) -> Result<Response<Body>, ProxyError> {
    // get the actual upstream URL from the proxied one
    let url = state
        .channels
        .get(&title)
        .ok_or_else(|| ProxyError::ChannelNotFound {
            title: title.clone(),
        })?
        .url
        .clone();

    let stream = attach_to_stream(&state, &title, &url).await?;

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "video/mp2t")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from_stream(stream))
        .map_err(|_| ProxyError::DownstreamSend {
            source: broadcast::error::SendError(Bytes::new()),
        })
}

/*
 * Only allow a guest to connect if there is already some stream initiated by someone else
 * i.e.: the admin controls which streams get initiated, and allows guests to attach to it
 */
pub async fn guest(State(state): State<AppState>) -> Result<Response<Body>, ProxyError> {
    let active_title = state.upstream_manager.stream_title.read().await.clone();
    let stream = match (
        active_title.as_deref(),
        state.upstream_manager.stream_url.read().await.as_deref(),
    ) {
        (Some(title), Some(url)) => attach_to_stream(&state, title, url).await,
        _ => Err(ProxyError::GuestError),
    }?;

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "video/mp2t")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from_stream(stream))
        .map_err(|_| ProxyError::DownstreamSend {
            source: broadcast::error::SendError(Bytes::new()),
        })
}

pub async fn guest_playlist(
    State(_): State<AppState>,
    headers: HeaderMap,
) -> Result<Response<Body>, ProxyError> {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");

    let body = serialize_guest_playlist(host);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/octet-stream")
        .header("Cache-Control", "no-cache")
        .body(Body::from(body.unwrap()))
        .map_err(|e| {
            tracing::error!("failed to build guest response: {}", e);
            ProxyError::GuestError
        })?)
}

async fn prewarm_channel(
    Path(title): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<PrewarmResponse>, StatusCode> {
    prewarm_upstream(&state, &title).await.map_err(|e| {
        tracing::warn!(channel = %title, error = %e, "prewarm failed");
        StatusCode::BAD_GATEWAY
    })?;

    Ok(Json(PrewarmResponse {
        success: true,
        message: format!("Stream prewarmed: {}", title),
    }))
}
