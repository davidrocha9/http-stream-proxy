use crate::proxy::UpstreamManager;
use crate::state::{AppConfig, AppState};
use crate::sync::SyncState;
use bytes::Bytes;
use dashmap::DashMap;
use m3u_parser::M3uParser;
use std::{io::Error, sync::Arc};
use tokio::sync::{RwLock, broadcast};
use urlencoding::encode;

// Sadly the m3u_parser crate swallows parsing errors completely
// We can't propagate them via api
pub async fn load_playlist() -> AppState {
    let config = AppConfig::load().expect("failed to load Config");
    let url = config.clone().upstream_m3u_url;

    // Create sync broadcast channel (capacity 16 is enough for state updates)
    let (sync_tx, _) = broadcast::channel::<SyncState>(16);

    let channels: DashMap<String, m3u_parser::Info> = DashMap::new();
    let mut channel_order: Vec<String> = Vec::new();
    let (sender, _) = broadcast::channel::<Bytes>(config.stream_broadcast_capacity.max(64));

    let mut playlist = M3uParser::new(None);
    playlist.parse_m3u(&url, false, false).await;
    for m in playlist.get_vector().iter() {
        channels.insert(m.title.clone(), m.clone());
        channel_order.push(m.title.clone());
    }

    let app_state = AppState {
        config,
        channels,
        channel_order: Arc::new(channel_order),
        upstream_manager: UpstreamManager::new(sender),
        sync_state: Arc::new(RwLock::new(SyncState::default())),
        sync_tx,
    };

    app_state
}

pub fn serialize_playlist(state: &AppState, host: &str) -> Result<bytes::Bytes, Error> {
    let mut out = String::with_capacity(state.channels.len() * 160);
    out.push_str("#EXTM3U\n");

    // Iterate in original playlist order
    for title in state.channel_order.iter() {
        let Some(entry) = state.channels.get(title) else {
            continue;
        };
        let info = entry.value();

        // EXTINF line
        out.push_str("#EXTINF:-1 ");

        out.push_str(&format!(
            r#"tvg-id="{}" tvg-name="{}" tvg-logo="{}" group-title="{}",{}"#,
            info.tvg.id, info.tvg.name, info.logo, info.category, info.title,
        ));

        out.push('\n');

        // Proxied URL - use the requesting host so it works via Tailscale or any hostname
        // The host header already includes the port if non-standard
        let proxied_url = format!("http://{}/channel/{}", host, encode(&info.title));

        out.push_str(&proxied_url);
        out.push('\n');
    }

    Ok(Bytes::from(out))
}

pub fn serialize_guest_playlist(host: &str) -> Result<bytes::Bytes, Error> {
    let mut out = String::with_capacity(128);
    out.push_str("#EXTM3U\n");

    // EXTINF line
    out.push_str("#EXTINF:-1 ");
    out.push_str(r#"tvg-id="live" tvg-name="live" tvg-logo="" group-title="Guest",live"#);
    out.push('\n');

    // Guest URL
    let guest_url = format!("http://{}/guest", host);
    out.push_str(&guest_url);
    out.push('\n');

    Ok(Bytes::from(out))
}
