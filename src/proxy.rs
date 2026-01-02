use crate::error::ProxyError;
use crate::state::AppState;
use bytes::Bytes;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct UpstreamManager {
    // TODO(caio): maybe we don't need this
    pub stream_url: Arc<RwLock<Option<String>>>,

    /*
     * This is static throughout the application's lifetimne
     * By keeping it static, consumers that subscribe to it will change streams
     * seamlessly
     */
    pub sender: broadcast::Sender<Bytes>,

    /*
     * we can only have ONE concurrent upstream connection at any time
     * so let's just have one single http client that can then easily be configured
     * e.g.: timeouts, headers, retries, etc
     */
    pub client: reqwest::Client,

    /*
     * We lock the `run` method so that we're not interleaving upstream connections
     */
    lock: Arc<tokio::sync::Mutex<()>>,

    // Cancellation token to signal the active upstream task to stop
    // When switching channels, we cancel the old task before starting the new one
    cancel_token: Arc<RwLock<CancellationToken>>,
}

impl UpstreamManager {
    pub fn new(sender: broadcast::Sender<Bytes>) -> Self {
        Self {
            sender,
            stream_url: Arc::new(RwLock::new(None)),
            client: reqwest::Client::new(),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            cancel_token: Arc::new(RwLock::new(CancellationToken::new())),
        }
    }

    // Cancel any active upstream connection
    async fn cancel_active(&self) {
        let token = self.cancel_token.read().await;
        token.cancel();
    }

    pub async fn run(&self, url: String, prewarm: bool) -> Result<(), ProxyError> {
        let _guard = self.lock.lock().await;

        // Check if we're already streaming this URL
        if let Some(active) = self.stream_url.read().await.as_deref() {
            if active == url {
                return Ok(());
            }
        }

        // Cancel any existing upstream connection before starting a new one
        self.cancel_active().await;

        // Create a new cancellation token for this connection
        let new_token = CancellationToken::new();
        let task_token = new_token.clone();
        {
            let mut token = self.cancel_token.write().await;
            *token = new_token;
        }

        let tx_clone = self.sender.clone();
        {
            let mut stream_url = self.stream_url.write().await;
            *stream_url = Some(url.clone());
        }

        tracing::info!("Starting connection to channel at url: {}", url);
        let parsed_url = reqwest::Url::parse(&url).expect("failed to parse stream url");
        let request = reqwest::Request::new(reqwest::Method::GET, parsed_url);

        let response = self
            .client
            .execute(request)
            .await
            .map_err(|e| ProxyError::UpstreamRequest { source: e })?;

        let stream = response.bytes_stream();
        let upstream_clone = self.clone();
        let mut chunk_cnt = 0;

        tokio::spawn(async move {
            // https://doc.rust-lang.org/std/pin/index.html#address-sensitive-values-aka-when-we-need-pinning:~:text=As%20a%20motivating,s.
            // tokio::select! needs the stream to be pinned since it relies on the reference not changing in
            // between poll_next() calls
            tokio::pin!(stream);

            loop {
                tokio::select! {
                    // Check for cancellation signal (channel switch)
                    // We don't need it, but it's a good idea to control our invariants
                    _ = task_token.cancelled() => {
                        tracing::info!("Upstream connection cancelled (channel switch): {}", url);
                        break;
                    }
                    chunk = stream.next() => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                chunk_cnt += 1;
                                tracing::debug!("Received {} bytes from upstream, {} receivers",
                                                    bytes.len(), tx_clone.receiver_count());

                                /*
                                 * usually we abort if the stream is closed
                                 * if we're prewarming, we have a little grace period (wait for a few chunks to arrive)
                                 * since the client needs a whole RTT to attach itself to the stream
                                 */
                                if tx_clone.receiver_count() == 0 || (prewarm && chunk_cnt <= 20) {
                                    tracing::debug!("No more subscribers for {}, closing stream", url);
                                    break;
                                }

                                if let Err(e) = tx_clone.send(bytes) {
                                    tracing::debug!(error = %e, "broadcast send failed (no receivers?)");
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(error = %e, "upstream stream error");
                                break;
                            }
                            None => {
                                tracing::debug!("Upstream stream ended: {}", url);
                                break;
                            }
                        }
                    }
                }
            }

            // Only clear stream_url if we weren't cancelled (i.e., stream ended naturally)
            // If cancelled, the new connection will set its own URL
            if !task_token.is_cancelled() {
                let mut stream_url = upstream_clone.stream_url.write().await;
                *stream_url = None;
            }

            tracing::debug!("Closing connection to url: {}", url);
        });

        Ok(())
    }
}

pub async fn attach_to_stream(
    state: &AppState,
    url: &str,
) -> Result<
    impl tokio_stream::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    ProxyError,
> {
    tracing::debug!("Attaching to stream at url {}", url);
    state.upstream_manager.run(url.to_string(), false).await?;
    let rx = state.upstream_manager.sender.clone().subscribe();

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

/*
 * Prewarm an upstream connection by starting the stream before any subscribers connect.
 * This reuses the UpstreamManager.run() method, so it benefits from the same
 * cancellation token mechanism and won't conflict with regular stream requests.
 */
pub async fn prewarm(state: &AppState, title: &str) -> Result<(), String> {
    let url = state
        .channels
        .get(title)
        .ok_or_else(|| format!("Channel not found: {}", title))?
        .url
        .clone();

    // If already streaming this URL, nothing to do
    if let Some(active) = state.upstream_manager.stream_url.read().await.as_deref() {
        if active == url {
            tracing::debug!(channel = %title, "prewarm skipped: already streaming this channel");
            return Ok(());
        }
    }

    tracing::info!(channel = %title, upstream_url = %url, "prewarm starting upstream connection");

    // Reuse the same run() method - this ensures proper cancellation handling
    // and uses the shared broadcast sender
    state
        .upstream_manager
        .run(url, true)
        .await
        .map_err(|e| e.to_string())
}
