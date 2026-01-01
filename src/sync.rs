use crate::proxy::prewarm_internal;
use crate::state::AppState;
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, time::Duration};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

// ============================================================================
// Data Types
// ============================================================================

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

// ============================================================================
// Core Sync Logic (reusable across handlers)
// ============================================================================

/// Check if the given channel URL differs from the current active channel
pub async fn is_channel_change(state: &AppState, channel_url: &str) -> bool {
    let current = state.sync_state.read().await;
    match &current.active_channel_url {
        Some(current_url) => current_url != channel_url,
        None => true, // No active channel means any channel is a change
    }
}

/// Update sync state and broadcast to all connected clients.
/// Returns the new SyncState.
pub async fn update_sync_state(
    state: &AppState,
    channel_url: Option<String>,
    channel_name: Option<String>,
    is_playing: Option<bool>,
    client_id: &str,
) -> SyncState {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let new_state = SyncState {
        active_channel_url: channel_url.clone(),
        active_channel_name: channel_name.clone(),
        is_playing: is_playing.unwrap_or(channel_url.is_some()),
        updated_at: now,
        updated_by: client_id.to_string(),
    };

    tracing::debug!(
        client_id = %client_id,
        channel_name = new_state.active_channel_name.as_deref().unwrap_or("stopped"),
        channel_url = new_state.active_channel_url.as_deref().unwrap_or("none"),
        is_playing = new_state.is_playing,
        "sync state updated"
    );

    // Update shared state
    {
        let mut sync_state = state.sync_state.write().await;
        *sync_state = new_state.clone();
    }

    // Broadcast to all connected clients
    if let Err(e) = state.sync_tx.send(new_state.clone()) {
        tracing::debug!("Broadcast send failed (no receivers): {}", e);
    }

    new_state
}

/// Clear sync state (stop playback) and broadcast to all connected clients.
/// Returns the new SyncState.
pub async fn clear_sync_state(state: &AppState, client_id: &str) -> SyncState {
    update_sync_state(state, None, None, Some(false), client_id).await
}

// ============================================================================
// API Handlers
// ============================================================================

/// SSE endpoint: GET /sync?clientId=...
/// Returns a stream of sync state updates
pub async fn sync_sse(
    Query(query): Query<SyncQuery>,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let client_id = query.client_id.unwrap_or_else(|| "unknown".to_string());
    tracing::info!(client_id = %client_id, "SSE client connected");

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
/// Also triggers prewarming if a new channel is selected
pub async fn sync_update(
    State(state): State<AppState>,
    Json(payload): Json<SyncUpdateRequest>,
) -> impl IntoResponse {
    // Trigger prewarm if we have a channel URL (before updating state)
    if let Some(ref url) = payload.active_channel_url {
        // Extract channel title from URL (e.g., /channel/my-title -> my-title)
        if let Some(title) = extract_channel_title(url) {
            if let Some(channel_info) = state.channels.get(&title) {
                let upstream_url = channel_info.url.clone();

                // Only prewarm if not already active
                if !state.streams.contains_key(&upstream_url) {
                    tracing::info!(channel = %title, "triggering prewarm");
                    // Fire-and-forget prewarm in background
                    let state_clone = state.clone();
                    let title_clone = title.clone();
                    tokio::spawn(async move {
                        if let Err(e) = prewarm_internal(&state_clone, &title_clone).await {
                            tracing::warn!(channel = %title_clone, error = %e, "prewarm failed");
                        }
                    });
                }
            }
        }
    }

    // Update and broadcast using shared logic
    update_sync_state(
        &state,
        payload.active_channel_url,
        payload.active_channel_name,
        payload.is_playing,
        &payload.client_id,
    )
    .await;

    StatusCode::OK
}

/// DELETE /sync - Clear sync state (stop playback for everyone)
pub async fn sync_delete(
    Query(query): Query<SyncDeleteQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let client_id = query.client_id.unwrap_or_else(|| "unknown".to_string());

    tracing::info!(client_id = %client_id, "sync stop requested");

    // Clear and broadcast using shared logic
    clear_sync_state(&state, &client_id).await;

    StatusCode::OK
}

// ============================================================================
// Internal Helpers
// ============================================================================

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
