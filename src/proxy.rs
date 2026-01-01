use crate::error::ProxyError;
use crate::state::AppState;
use bytes::Bytes;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;

async fn spawn_upstream(state: &AppState, url: String) -> Result<Sender<Bytes>, ProxyError> {
    let (tx, _) = broadcast::channel::<Bytes>(1024);
    let tx_clone = tx.clone();
    let state_handle = Arc::clone(&state.streams);

    state.streams.insert(url.clone(), tx.clone());
    tracing::info!("Starting connection to channel at url: {}", url);
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

    Ok(tx)
}

pub async fn attach_to_stream(
    state: &AppState,
    url: String,
) -> Result<
    impl tokio_stream::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    ProxyError,
> {
    let tx = match state.streams.get(url.as_str()) {
        Some(tx) => tx.clone(),
        None => spawn_upstream(state, url)
            .await
            .expect("spawn_upstream failed"),
    };

    let rx = tx.subscribe();

    let stream = async_stream::stream! {
        let mut rx_stream = tokio_stream::wrappers::BroadcastStream::new(rx);

        while let Some(item) = rx_stream.next().await {
            match item {
                Ok(bytes) => yield Ok(bytes),
                Err(_) => tracing::warn!("broadcast consumer lagged or closed"),
            }
        }
    };

    Ok(stream)
}

pub async fn prewarm_internal(state: &AppState, title: &str) -> Result<(), String> {
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
    tracing::info!(channel = %title, upstream_url = %url, "prewarm starting upstream connection");

    let parsed_url = reqwest::Url::parse(&url).map_err(|e| e.to_string())?;
    let request = reqwest::Request::new(reqwest::Method::GET, parsed_url);

    let response = state
        .client
        .execute(request)
        .await
        .map_err(|e| e.to_string())?;

    let mut stream = futures_util::StreamExt::fuse(response.bytes_stream());

    // Spawn background task to keep the stream warm until subscribers connect or timeout
    tokio::spawn(async move {
        let mut chunk_count = 0;
        let prewarm_timeout = tokio::time::sleep(tokio::time::Duration::from_secs(10));
        tokio::pin!(prewarm_timeout);

        loop {
            tokio::select! {
                _ = &mut prewarm_timeout => {
                    if tx_clone.receiver_count() == 0 {
                        tracing::info!(channel = %title_owned, "prewarm timeout with no subscribers, closing");
                        break;
                    }
                    // If we have subscribers, continue indefinitely (normal stream behavior)
                }
                chunk = tokio_stream::StreamExt::next(&mut stream) => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            chunk_count += 1;
                            if chunk_count == 1 {
                                tracing::debug!(channel = %title_owned, "prewarm first chunk received");
                            }

                            // Once subscribers connect, check if they all disconnect
                            if tx_clone.receiver_count() == 0 && chunk_count > 20 {
                                // Only exit if we HAD subscribers and they all left
                                // The timeout handles the "never had subscribers" case
                                if prewarm_timeout.is_elapsed() {
                                    tracing::debug!(channel = %title_owned, "no more subscribers, closing prewarm");
                                    break;
                                }
                            }

                            if let Err(e) = tx_clone.send(bytes) {
                                tracing::trace!(error = %e, "prewarm broadcast send failed (no receivers)");
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(channel = %title_owned, error = %e, "upstream stream error during prewarm");
                            break;
                        }
                        None => {
                            tracing::debug!(channel = %title_owned, "upstream stream ended");
                            break;
                        }
                    }
                }
            }
        }

        tracing::debug!(channel = %title_owned, "prewarm connection closed");
        state_handle.remove(&url_clone);
    });

    Ok(())
}
