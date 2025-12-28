use bytes::Bytes;
use config::{Config, Environment, File};
use dashmap::DashMap;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast;
use m3u_parser::Info;

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
    // static, constructed at startup time - no shared ownership needed
    pub channels: DashMap<String, Info>, // title -> Stream info
    pub streams: Arc<DashMap<String, broadcast::Sender<Bytes>>>, // URL -> Active Broadcast
    /*
     * we can only have ONE concurrent upstream connection at any time
     * so let's just have one single http client that can then easily be configured
     * e.g.: timeouts, headers, retries, etc
     */
    pub client: reqwest::Client,
}
