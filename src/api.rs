use crate::error::{ProxyError, UpstreamFetchError};
use crate::sync::{sync_sse, sync_update, sync_delete};
use crate::playlist::{load_playlist, serialize_playlist};
use crate::proxy::attach_to_stream;
use crate::sync::{is_channel_change, update_sync_state};
use crate::state::AppState;
use axum::http::HeaderMap;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::Response,
    routing::get,
    routing::post,
    routing::delete,
};
use bytes::Bytes;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use urlencoding::encode;

pub async fn serve() {
    let app_state = load_playlist().await;
    let app_state_clone = app_state.clone();

    let cors = CorsLayer::permissive(); // TODO(caio): make cors configurable - only allow origin from config

    //TODO (caio): add tracing layer middleware
    let app = Router::new()
        .route("/", get(load))
        .route("/channel/{title}", get(proxy))
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
        .body(Body::from(body.unwrap())) // ← this is the only step needed
        .map_err(|e| {
            tracing::error!("failed to build response: {}", e);
            UpstreamFetchError::ResponseBuild { source: e }
        })?)
}

async fn proxy(
    Path(title): Path<String>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Response<Body>, ProxyError> {
    let url = state
        .channels
        .get(&title)
        .ok_or_else(|| ProxyError::ChannelNotFound {
            title: title.clone(),
        })?
        .url
        .clone();

    // Build the channel URL for sync state (matches playlist format)
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let channel_url = format!("http://{}/channel/{}", host, encode(&title));

    // Broadcast sync state if this is a channel change
    if is_channel_change(&state, &channel_url).await {
        let client_id = headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        update_sync_state(
            &state,
            Some(channel_url),
            Some(title.clone()),
            Some(true),
            &client_id,
        )
        .await;
    }

    let stream = attach_to_stream(&state, &url)
        .await
        .expect("failed to attach to stream");

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "video/mp2t")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from_stream(stream))
        .map_err(|_| ProxyError::DownstreamSend {
            source: broadcast::error::SendError(Bytes::new()),
        })
}
