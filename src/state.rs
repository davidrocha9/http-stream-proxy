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

    // Sync state for SSE broadcast to all clients (deprecated)
    pub sync_state: Arc<RwLock<SyncState>>,
    pub sync_tx: broadcast::Sender<SyncState>,
}
