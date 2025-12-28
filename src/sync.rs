use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, time::Duration};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use crate::state::AppState;

/// Sync state shared across all clients
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncState {
    pub active_channel_url: Option<String>,
    pub active_channel_name: Option<String>,
    pub is_playing: bool,
    pub updated_at: u64,
    pub updated_by: String,
}

/// Query params for SSE connection
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncQuery {
    pub client_id: Option<String>,
}

/// POST body for sync updates
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncUpdateRequest {
    pub active_channel_url: Option<String>,
    pub active_channel_name: Option<String>,
    pub is_playing: Option<bool>,
    pub client_id: String,
}

/// DELETE query params
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncDeleteQuery {
    pub client_id: Option<String>,
}

/// SSE endpoint: GET /sync?clientId=...
/// Returns a stream of sync state updates
pub async fn sync_sse(
    Query(query): Query<SyncQuery>,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let client_id = query.client_id.unwrap_or_else(|| "unknown".to_string());
    println!("[Sync] Client connected: {}", client_id);

    // Get current state to send immediately
    let current = state.sync_state.read().await.clone();

    // Subscribe to broadcast channel for future updates
    let rx = state.sync_tx.subscribe();

    let stream = async_stream::stream! {
        // Send current state immediately on connect
        if let Ok(json) = serde_json::to_string(&current) {
            yield Ok(Event::default().data(json));
        }

        // Forward all broadcast updates
        let mut rx_stream = BroadcastStream::new(rx);
        while let Some(result) = rx_stream.next().await {
            if let Ok(sync_state) = result {
                if let Ok(json) = serde_json::to_string(&sync_state) {
                    yield Ok(Event::default().data(json));
                }
            }
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

/// POST /sync - Update sync state and broadcast to all clients
pub async fn sync_update(
    State(state): State<AppState>,
    Json(payload): Json<SyncUpdateRequest>,
) -> impl IntoResponse {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let new_state = SyncState {
        active_channel_url: payload.active_channel_url.clone(),
        active_channel_name: payload.active_channel_name.clone(),
        is_playing: payload.is_playing.unwrap_or(payload.active_channel_url.is_some()),
        updated_at: now,
        updated_by: payload.client_id.clone(),
    };

    println!(
        "[Sync] Update from {}: {} ({})",
        payload.client_id,
        new_state.active_channel_name.as_deref().unwrap_or("stopped"),
        new_state.active_channel_url.as_deref().unwrap_or("none")
    );

    // Trigger prewarm if we have a channel URL
    if let Some(ref url) = new_state.active_channel_url {
        // Extract channel title from URL (e.g., /channel/my-title -> my-title)
        if let Some(title) = extract_channel_title(url) {
            if let Some(channel_info) = state.channels.get(&title) {
                let upstream_url = channel_info.url.clone();
                
                // Only prewarm if not already active
                if !state.streams.contains_key(&upstream_url) {
                    println!("[Sync] Triggering prewarm for: {}", title);
                    // Fire-and-forget prewarm in background
                    let state_clone = state.clone();
                    let title_clone = title.clone();
                    tokio::spawn(async move {
                        if let Err(e) = prewarm_internal(&state_clone, &title_clone).await {
                            tracing::warn!("Prewarm failed for {}: {:?}", title_clone, e);
                        }
                    });
                }
            }
        }
    }

    // Update shared state
    {
        let mut sync_state = state.sync_state.write().await;
        *sync_state = new_state.clone();
    }

    // Broadcast to all connected clients
    if let Err(e) = state.sync_tx.send(new_state) {
        tracing::debug!("Broadcast send failed (no receivers): {}", e);
    }

    StatusCode::OK
}

/// DELETE /sync - Clear sync state (stop playback for everyone)
pub async fn sync_delete(
    Query(query): Query<SyncDeleteQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let client_id = query.client_id.unwrap_or_else(|| "unknown".to_string());
    
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let new_state = SyncState {
        active_channel_url: None,
        active_channel_name: None,
        is_playing: false,
        updated_at: now,
        updated_by: client_id.clone(),
    };

    println!("[Sync] Stop from {}", client_id);

    // Update shared state
    {
        let mut sync_state = state.sync_state.write().await;
        *sync_state = new_state.clone();
    }

    // Broadcast to all connected clients
    if let Err(e) = state.sync_tx.send(new_state) {
        tracing::debug!("Broadcast send failed (no receivers): {}", e);
    }

    StatusCode::OK
}

/// Extract channel title from a URL like "http://proxy:2727/channel/my-title"
fn extract_channel_title(url: &str) -> Option<String> {
    // Try to extract from /channel/{title} pattern
    if let Some(pos) = url.find("/channel/") {
        let rest = &url[pos + 9..]; // skip "/channel/"
        // Take until next slash or end
        let title = rest.split('/').next().unwrap_or(rest);
        if !title.is_empty() {
            return Some(urlencoding::decode(title).unwrap_or_default().into_owned());
        }
    }
    None
}

/// Internal prewarm function (reused from api.rs logic)
async fn prewarm_internal(state: &AppState, title: &str) -> Result<(), String> {
    let url = state
        .channels
        .get(title)
        .ok_or_else(|| format!("Channel not found: {}", title))?
        .url
        .clone();

    if state.streams.contains_key(&url) {
        return Ok(()); // Already active
    }

    let (tx, _) = tokio::sync::broadcast::channel::<bytes::Bytes>(1024);
    let tx_clone = tx.clone();
    let state_handle = std::sync::Arc::clone(&state.streams);
    let url_clone = url.clone();
    let title_owned = title.to_string();

    state.streams.insert(url.clone(), tx);
    println!("[Prewarm/Sync] Starting connection to channel: {} at url: {}", title, url);

    let parsed_url = reqwest::Url::parse(&url).map_err(|e| e.to_string())?;
    let request = reqwest::Request::new(reqwest::Method::GET, parsed_url);

    let response = state
        .client
        .execute(request)
        .await
        .map_err(|e| e.to_string())?;

    let mut stream = futures_util::StreamExt::fuse(response.bytes_stream());

    // Spawn background task to keep the stream warm
    tokio::spawn(async move {
        let mut chunk_count = 0;
        let prewarm_timeout = tokio::time::sleep(tokio::time::Duration::from_secs(30));
        tokio::pin!(prewarm_timeout);

        loop {
            tokio::select! {
                _ = &mut prewarm_timeout => {
                    if tx_clone.receiver_count() == 0 {
                        println!("[Prewarm/Sync] Timeout with no subscribers for {}, closing", title_owned);
                        break;
                    }
                }
                chunk = tokio_stream::StreamExt::next(&mut stream) => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            chunk_count += 1;
                            if chunk_count == 1 {
                                println!("[Prewarm/Sync] First chunk received for {}", title_owned);
                            }
                            
                            if tx_clone.receiver_count() == 0 && chunk_count > 10 {
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
                            break;
                        }
                    }
                }
            }
        }

        state_handle.remove(&url_clone);
    });

    Ok(())
}
