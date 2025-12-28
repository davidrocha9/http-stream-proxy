use crate::error::{ProxyError, UpstreamFetchError};
use crate::playlist::{deserialize_playlist, load_playlist};
use crate::state::AppState;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::Response,
    routing::get,
};
use bytes::Bytes;
use futures_util::StreamExt;
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
async fn load(State(state): State<AppState>) -> Result<Response<Body>, UpstreamFetchError> {
    let body = deserialize_playlist(&state); // bytes::Bytes

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
