use config::{Config, Environment, File};
use dashmap::DashMap;
use m3u_parser::Info;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

use crate::proxy::UpstreamManager;
use crate::sync::SyncState;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub upstream_m3u_url: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_transcode_full_hd_audio_to_aac")]
    pub transcode_full_hd_audio_to_aac: bool,
    #[serde(default = "default_transcode_full_hd_video_to_h264")]
    pub transcode_full_hd_video_to_h264: bool,
    #[serde(default = "default_ffmpeg_path")]
    pub ffmpeg_path: String,
    #[serde(default = "default_ffmpeg_audio_bitrate_kbps")]
    pub ffmpeg_audio_bitrate_kbps: u32,
    #[serde(default = "default_ffmpeg_video_preset")]
    pub ffmpeg_video_preset: String,
    #[serde(default = "default_ffmpeg_video_crf")]
    pub ffmpeg_video_crf: u32,
    #[serde(default = "default_ffmpeg_video_encoder")]
    pub ffmpeg_video_encoder: String,
    #[serde(default = "default_ffmpeg_video_threads")]
    pub ffmpeg_video_threads: u32,
    #[serde(default = "default_ffmpeg_video_output_fps")]
    pub ffmpeg_video_output_fps: u32,
    #[serde(default = "default_ffmpeg_video_maxrate_kbps")]
    pub ffmpeg_video_maxrate_kbps: u32,
    #[serde(default = "default_ffmpeg_video_bufsize_kbps")]
    pub ffmpeg_video_bufsize_kbps: u32,
    #[serde(default = "default_stream_broadcast_capacity")]
    pub stream_broadcast_capacity: usize,
    #[serde(default = "default_transcoded_read_chunk_bytes")]
    pub transcoded_read_chunk_bytes: usize,
    #[serde(default = "default_prewarm_keepalive_secs")]
    pub prewarm_keepalive_secs: u64,
    #[serde(default = "default_prewarm_transcoded_streams")]
    pub prewarm_transcoded_streams: bool,
    #[serde(default = "default_admin_email")]
    pub admin_email: String,
    #[serde(default)]
    pub whitelisted_emails: Vec<String>,
    #[serde(default = "default_env")]
    pub env: String,
}

impl AppConfig {
    pub fn load() -> Result<Self, config::ConfigError> {
        let settings = Config::builder()
            // load JSONC file (JSON with comments)
            .add_source(File::with_name("proxy.json5").required(true))
            // merge environment variables, prefixed with `APP_`, e.g., `APP_UPSTREAM_M3U_URL`
            .add_source(Environment::with_prefix("APP").separator("_"))
            .build()?;

        settings.try_deserialize::<AppConfig>()
    }
}

fn default_port() -> u16 {
    2727
}

fn default_transcode_full_hd_audio_to_aac() -> bool {
    true
}

fn default_transcode_full_hd_video_to_h264() -> bool {
    true
}

fn default_ffmpeg_path() -> String {
    "ffmpeg".to_string()
}

fn default_ffmpeg_audio_bitrate_kbps() -> u32 {
    128
}

fn default_ffmpeg_video_preset() -> String {
    "veryfast".to_string()
}

fn default_ffmpeg_video_crf() -> u32 {
    23
}

fn default_ffmpeg_video_encoder() -> String {
    "libx264".to_string()
}

fn default_ffmpeg_video_threads() -> u32 {
    2
}

fn default_ffmpeg_video_output_fps() -> u32 {
    25
}

fn default_ffmpeg_video_maxrate_kbps() -> u32 {
    4500
}

fn default_ffmpeg_video_bufsize_kbps() -> u32 {
    2250
}

fn default_stream_broadcast_capacity() -> usize {
    512
}

fn default_transcoded_read_chunk_bytes() -> usize {
    32 * 1024
}

fn default_prewarm_keepalive_secs() -> u64 {
    8
}

fn default_prewarm_transcoded_streams() -> bool {
    false
}

fn default_admin_email() -> String {
    "".to_string()
}

fn default_env() -> String {
    "local".to_string()
}

/*
 * The channel title will be the entrypoint for the stream
 * i.e.: with the title, we identify the upstream URL
 *
 * Each upstream URL will be linked to its downstream active consumers
 * through the streams DashMap
 * NOTE(caio): DashMap is an implementation of concurrent hash map that
 * allows for wrapping the hashmap directly into an ARC
 */

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub channels: DashMap<String, Info>, // title -> Stream info
    pub channel_order: Arc<Vec<String>>, // preserves original playlist order
    pub upstream_manager: UpstreamManager,

    // Sync state for SSE broadcast to all clients
    pub sync_state: Arc<RwLock<SyncState>>,
    pub sync_tx: broadcast::Sender<SyncState>,
}
