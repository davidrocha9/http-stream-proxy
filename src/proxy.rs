use crate::error::ProxyError;
use crate::state::{AppConfig, AppState};
use bytes::Bytes;
use futures_util::StreamExt;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{RwLock, broadcast};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug)]
struct TranscodeMode {
    audio_to_aac: bool,
    video_to_h264: bool,
}

impl TranscodeMode {
    fn passthrough() -> Self {
        Self {
            audio_to_aac: false,
            video_to_h264: false,
        }
    }

    fn is_enabled(self) -> bool {
        self.audio_to_aac || self.video_to_h264
    }
}

fn transcode_mode_for_channel(config: &AppConfig, title: &str) -> TranscodeMode {
    let lowered = title.to_ascii_lowercase();
    let is_full_hd =
        lowered.contains("full hd") || lowered.contains("fhd") || lowered.contains("1080");

    if !is_full_hd {
        return TranscodeMode::passthrough();
    }

    TranscodeMode {
        audio_to_aac: config.transcode_full_hd_audio_to_aac,
        video_to_h264: config.transcode_full_hd_video_to_h264,
    }
}

fn stream_key(url: &str, mode: TranscodeMode) -> String {
    format!(
        "{}:a{}:v{}",
        url,
        if mode.audio_to_aac { 1 } else { 0 },
        if mode.video_to_h264 { 1 } else { 0 }
    )
}

#[derive(Clone)]
pub struct UpstreamManager {
    pub stream_url: Arc<RwLock<Option<String>>>,
    pub stream_title: Arc<RwLock<Option<String>>>,

    /*
     * This is static throughout the application's lifetime
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

    // Active stream identity (URL + transcode mode) to avoid unnecessary restarts.
    stream_key: Arc<RwLock<Option<String>>>,
}

impl UpstreamManager {
    pub fn new(sender: broadcast::Sender<Bytes>) -> Self {
        Self {
            sender,
            stream_url: Arc::new(RwLock::new(None)),
            stream_title: Arc::new(RwLock::new(None)),
            client: reqwest::Client::new(),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            cancel_token: Arc::new(RwLock::new(CancellationToken::new())),
            stream_key: Arc::new(RwLock::new(None)),
        }
    }

    async fn clear_if_active(&self, key: &str) {
        let is_active = self.stream_key.read().await.as_deref() == Some(key);
        if !is_active {
            return;
        }

        {
            let mut stream_key = self.stream_key.write().await;
            if stream_key.as_deref() == Some(key) {
                *stream_key = None;
            }
        }

        {
            let mut stream_url = self.stream_url.write().await;
            if stream_url.is_some() {
                *stream_url = None;
            }
        }

        {
            let mut stream_title = self.stream_title.write().await;
            if stream_title.is_some() {
                *stream_title = None;
            }
        }
    }

    // Cancel any active upstream connection
    async fn cancel_active(&self) {
        let token = self.cancel_token.read().await;
        token.cancel();
    }

    pub async fn run(
        &self,
        title: String,
        url: String,
        prewarm: bool,
        config: AppConfig,
    ) -> Result<(), ProxyError> {
        let _guard = self.lock.lock().await;
        let mode = transcode_mode_for_channel(&config, &title);
        let key = stream_key(&url, mode);

        // Check if we're already streaming this exact source/mode.
        if self.stream_key.read().await.as_deref() == Some(key.as_str()) {
            return Ok(());
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

        {
            let mut stream_key = self.stream_key.write().await;
            *stream_key = Some(key.clone());
        }
        {
            let mut stream_url = self.stream_url.write().await;
            *stream_url = Some(url.clone());
        }
        {
            let mut stream_title = self.stream_title.write().await;
            *stream_title = Some(title.clone());
        }

        let start_result = if mode.is_enabled() {
            self.start_transcoded(title, url, key.clone(), prewarm, config, task_token)
                .await
        } else {
            self.start_passthrough(
                url,
                key.clone(),
                prewarm,
                config.prewarm_keepalive_secs,
                task_token,
            )
            .await
        };

        if let Err(err) = start_result {
            self.clear_if_active(&key).await;
            return Err(err);
        }

        Ok(())
    }

    async fn start_passthrough(
        &self,
        url: String,
        key: String,
        prewarm: bool,
        prewarm_keepalive_secs: u64,
        task_token: CancellationToken,
    ) -> Result<(), ProxyError> {
        tracing::info!("Starting passthrough connection to channel at url: {}", url);
        let parsed_url = reqwest::Url::parse(&url).expect("failed to parse stream url");
        let request = reqwest::Request::new(reqwest::Method::GET, parsed_url);

        let response = self
            .client
            .execute(request)
            .await
            .map_err(|e| ProxyError::UpstreamRequest { source: e })?
            .error_for_status()
            .map_err(|e| ProxyError::UpstreamRequest { source: e })?;

        let stream = response.bytes_stream();
        let tx_clone = self.sender.clone();
        let upstream_clone = self.clone();

        tokio::spawn(async move {
            tokio::pin!(stream);
            let prewarm_deadline = tokio::time::Instant::now()
                + tokio::time::Duration::from_secs(prewarm_keepalive_secs.max(1));

            loop {
                tokio::select! {
                    _ = task_token.cancelled() => {
                        tracing::info!("Passthrough upstream cancelled (channel switch): {}", url);
                        break;
                    }
                    chunk = stream.next() => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                let no_subscribers = tx_clone.receiver_count() == 0;
                                let within_prewarm_keepalive =
                                    prewarm && tokio::time::Instant::now() < prewarm_deadline;
                                if no_subscribers && !within_prewarm_keepalive {
                                    tracing::debug!("No subscribers left for {}, closing passthrough stream", url);
                                    break;
                                }

                                if let Err(e) = tx_clone.send(bytes) {
                                    tracing::debug!(error = %e, "broadcast send failed (no receivers?)");
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(error = %e, "upstream passthrough stream error");
                                break;
                            }
                            None => {
                                tracing::debug!("Upstream passthrough stream ended: {}", url);
                                break;
                            }
                        }
                    }
                }
            }

            upstream_clone.clear_if_active(&key).await;
            tracing::debug!("Closing passthrough connection to url: {}", url);
        });

        Ok(())
    }

    async fn start_transcoded(
        &self,
        title: String,
        url: String,
        key: String,
        prewarm: bool,
        config: AppConfig,
        task_token: CancellationToken,
    ) -> Result<(), ProxyError> {
        tracing::info!(
            channel = %title,
            audio_to_aac = config.transcode_full_hd_audio_to_aac,
            video_to_h264 = config.transcode_full_hd_video_to_h264,
            "Starting transcoded upstream connection"
        );

        let mode = transcode_mode_for_channel(&config, &title);
        let mut cmd = Command::new(&config.ffmpeg_path);
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostats")
            .arg("-fflags")
            .arg("+discardcorrupt")
            .arg("-err_detect")
            .arg("ignore_err")
            .arg("-analyzeduration")
            .arg("0")
            .arg("-probesize")
            .arg("64k")
            .arg("-i")
            .arg(&url)
            .arg("-map")
            .arg("0:v:0")
            .arg("-map")
            .arg("0:a:0?");

        if mode.video_to_h264 {
            cmd.arg("-c:v").arg(&config.ffmpeg_video_encoder);

            if config.ffmpeg_video_output_fps > 0 {
                cmd.arg("-r")
                    .arg(config.ffmpeg_video_output_fps.to_string());
            }

            if config.ffmpeg_video_threads > 0 {
                cmd.arg("-threads")
                    .arg(config.ffmpeg_video_threads.to_string());
            }

            cmd.arg("-bf")
                .arg("0")
                .arg("-maxrate")
                .arg(format!("{}k", config.ffmpeg_video_maxrate_kbps))
                .arg("-bufsize")
                .arg(format!("{}k", config.ffmpeg_video_bufsize_kbps));

            if config.ffmpeg_video_encoder == "libx264" {
                let gop = (config.ffmpeg_video_output_fps.max(25) * 2).to_string();
                cmd.arg("-preset")
                    .arg(&config.ffmpeg_video_preset)
                    .arg("-crf")
                    .arg(config.ffmpeg_video_crf.to_string())
                    .arg("-tune")
                    .arg("zerolatency")
                    .arg("-pix_fmt")
                    .arg("yuv420p")
                    .arg("-profile:v")
                    .arg("high")
                    .arg("-level:v")
                    .arg("4.0")
                    .arg("-g")
                    .arg(&gop)
                    .arg("-keyint_min")
                    .arg(gop)
                    .arg("-sc_threshold")
                    .arg("0");
            } else if config.ffmpeg_video_encoder == "h264_v4l2m2m" {
                cmd.arg("-pix_fmt").arg("yuv420p");
            }
        } else {
            cmd.arg("-c:v").arg("copy");
        }

        if mode.audio_to_aac {
            cmd.arg("-c:a")
                .arg("aac")
                .arg("-ac")
                .arg("2")
                .arg("-ar")
                .arg("48000")
                .arg("-b:a")
                .arg(format!("{}k", config.ffmpeg_audio_bitrate_kbps));
        } else {
            cmd.arg("-c:a").arg("copy");
        }

        let mut child = cmd
            .arg("-muxdelay")
            .arg("0")
            .arg("-muxpreload")
            .arg("0")
            .arg("-flush_packets")
            .arg("1")
            .arg("-f")
            .arg("mpegts")
            .arg("pipe:1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| ProxyError::TranscoderStart { source: e })?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProxyError::TranscoderStart {
                source: std::io::Error::other("ffmpeg stdout not piped"),
            })?;

        let tx_clone = self.sender.clone();
        let upstream_clone = self.clone();
        let read_chunk_bytes = config.transcoded_read_chunk_bytes.max(4096);

        tokio::spawn(async move {
            let mut buffer = vec![0u8; read_chunk_bytes];
            let prewarm_deadline = tokio::time::Instant::now()
                + tokio::time::Duration::from_secs(config.prewarm_keepalive_secs.max(1));

            loop {
                tokio::select! {
                    _ = task_token.cancelled() => {
                        tracing::info!("Transcoded upstream cancelled (channel switch): {}", title);
                        break;
                    }
                    read = stdout.read(&mut buffer) => {
                        match read {
                            Ok(0) => {
                                tracing::debug!("ffmpeg output ended for {}", title);
                                break;
                            }
                            Ok(read_bytes) => {
                                let no_subscribers = tx_clone.receiver_count() == 0;
                                let within_prewarm_keepalive =
                                    prewarm && tokio::time::Instant::now() < prewarm_deadline;
                                if no_subscribers && !within_prewarm_keepalive {
                                    tracing::debug!("No subscribers left for {}, stopping transcoder", title);
                                    break;
                                }

                                if let Err(e) = tx_clone.send(Bytes::copy_from_slice(&buffer[..read_bytes])) {
                                    tracing::debug!(error = %e, "broadcast send failed");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "transcoded stream read error");
                                break;
                            }
                        }
                    }
                }
            }

            if let Err(e) = child.kill().await {
                tracing::debug!(error = %e, "failed to kill ffmpeg child");
            }
            let _ = child.wait().await;

            upstream_clone.clear_if_active(&key).await;
            tracing::debug!("Closing transcoded connection to url: {}", url);
        });

        Ok(())
    }
}

pub async fn attach_to_stream(
    state: &AppState,
    title: &str,
    url: &str,
) -> Result<
    impl tokio_stream::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    ProxyError,
> {
    tracing::debug!("Attaching to stream at url {}", url);
    state
        .upstream_manager
        .run(
            title.to_string(),
            url.to_string(),
            false,
            state.config.clone(),
        )
        .await?;
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
    let mode = transcode_mode_for_channel(&state.config, title);

    if mode.is_enabled() && !state.config.prewarm_transcoded_streams {
        tracing::debug!(channel = %title, "prewarm skipped for transcoded stream");
        return Ok(());
    }

    // If already streaming this URL/mode, nothing to do
    if state.upstream_manager.stream_key.read().await.as_deref()
        == Some(stream_key(&url, mode).as_str())
    {
        tracing::debug!(channel = %title, "prewarm skipped: already streaming this channel");
        return Ok(());
    }

    tracing::info!(channel = %title, upstream_url = %url, "prewarm starting upstream connection");
    state
        .upstream_manager
        .run(title.to_string(), url, true, state.config.clone())
        .await
        .map_err(|e| e.to_string())
}
