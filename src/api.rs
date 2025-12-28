use crate::error::{ProxyError, UpstreamFetchError};
use crate::playlist::{deserialize_playlist, load_playlist};
use crate::state::AppState;
use crate::sync::{sync_delete, sync_sse, sync_update};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use axum::http::HeaderMap;
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;

pub async fn serve() {
    let app_state = load_playlist().await;
    let app_state_clone = app_state.clone();

    let cors = CorsLayer::permissive(); //TODO(caio): make cors configurable - only allow origin from config

    //TODO (caio): add tracing layer
    let app = Router::new()
        .route("/", get(load))
        .route("/channel/{title}", get(proxy))
        .route("/prewarm/{title}", post(prewarm))
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

    let body = deserialize_playlist(&state, &host); // bytes::Bytes

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

    let tx = match state.streams.get(&url) {
        Some(tx) => tx.clone(),
        None => {
            let (tx, _) = broadcast::channel::<Bytes>(1024);
            let tx_clone = tx.clone();
            let state_handle = Arc::clone(&state.streams);

            state.streams.insert(url.clone(), tx.clone());
            println!("Starting connection to channel at url: {}", url);
            let parsed_url = reqwest::Url::parse(&url).expect("failed to parse stream url");
            let request = reqwest::Request::new(reqwest::Method::GET, parsed_url);

            let response = state
                .client
                .execute(request)
                .await
                .map_err(|e| ProxyError::UpstreamRequest { source: e })?;

            let mut stream = response.bytes_stream();

            tokio::spawn(async move {
                while let Some(chunk) = stream.next().await {
                    // Check if there are active subscribers to the broadcast channel
                    if tx_clone.receiver_count() == 0 {
                        tracing::debug!("No more subscribers for {}, closing stream", url);
                        break;
                    }

                    match chunk {
                        Ok(bytes) => {
                            if let Err(e) = tx_clone.send(bytes) {
                                // If send fails (no receivers), it's fine, we break on next loop via receiver_count check or here
                                tracing::debug!(error = %e, "broadcast send failed (no receivers?)");
                                // Actually send error usually means no receivers for broadcast? 
                                // Broadcast channel returns error only if closed.
                                // But we hold a clone of tx, so it's not closed.
                                // receiver_count() is the better check.
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "upstream stream error");
                            break;
                        }
                    }
                }

                tracing::debug!("Closing connection to url: {}", url);
                state_handle.remove(&url);
            });

            tx
        }
    };

    let rx = tx.subscribe();

    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|res| async { res.ok() })
        .map(Ok::<_, std::io::Error>);

    // http streaming response to the downstream
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "video/mp2t")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from_stream(stream))
        .map_err(|_| ProxyError::DownstreamSend {
            source: broadcast::error::SendError(Bytes::new()),
        })
}

#[derive(Serialize)]
struct PrewarmResponse {
    success: bool,
    message: String,
    already_active: bool,
}

/// Prewarm a channel by starting the upstream connection before clients request it.
/// This allows all synced clients to have the stream ready immediately.
async fn prewarm(
    Path(title): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ProxyError> {
    let url = state
        .channels
        .get(&title)
        .ok_or_else(|| ProxyError::ChannelNotFound {
            title: title.clone(),
        })?
        .url
        .clone();

    if state.streams.contains_key(&url) {
        return Ok(Json(PrewarmResponse {
            success: true,
            message: format!("Stream already active: {}", title),
            already_active: true,
        }));
    }

    let (tx, _) = broadcast::channel::<Bytes>(1024);
    let tx_clone = tx.clone();
    let state_handle = Arc::clone(&state.streams);
    let url_clone = url.clone();

    state.streams.insert(url.clone(), tx);
    println!("[Prewarm] Starting connection to channel: {} at url: {}", title, url);
    
    let parsed_url = reqwest::Url::parse(&url).expect("failed to parse stream url");
    let request = reqwest::Request::new(reqwest::Method::GET, parsed_url);

    let response = state
        .client
        .execute(request)
        .await
        .map_err(|e| ProxyError::UpstreamRequest { source: e })?;

    let mut stream = response.bytes_stream();

    // Spawn background task to keep the stream warm
    tokio::spawn(async move {
        let mut chunk_count = 0;
        let prewarm_timeout = tokio::time::sleep(tokio::time::Duration::from_secs(30));
        tokio::pin!(prewarm_timeout);

        loop {
            tokio::select! {
                // Timeout if no subscribers connect within 30 seconds
                _ = &mut prewarm_timeout => {
                    if tx_clone.receiver_count() == 0 {
                        println!("[Prewarm] Timeout with no subscribers for {}, closing", url_clone);
                        break;
                    }
                    // Reset timeout if we have subscribers
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            chunk_count += 1;
                            if chunk_count == 1 {
                                println!("[Prewarm] First chunk received for {}", url_clone);
                            }
                            
                            // Check if there are active subscribers
                            if tx_clone.receiver_count() == 0 && chunk_count > 10 {
                                tracing::debug!("No subscribers for {}, closing prewarmed stream", url_clone);
                                break;
                            }

                            if let Err(e) = tx_clone.send(bytes) {
                                tracing::debug!(error = %e, "broadcast send failed");
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "upstream stream error during prewarm");
                            break;
                        }
                        None => {
                            tracing::debug!("Upstream stream ended for {}", url_clone);
                            break;
                        }
                    }
                }
            }
        }

        tracing::debug!("Closing prewarmed connection to url: {}", url_clone);
        state_handle.remove(&url_clone);
    });

    Ok(Json(PrewarmResponse {
        success: true,
        message: format!("Stream prewarmed: {}", title),
        already_active: false,
    }))
}
