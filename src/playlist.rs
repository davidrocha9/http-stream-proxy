use crate::state::{AppConfig, AppState};
use bytes::Bytes;
use dashmap::DashMap;
use m3u_parser::M3uParser;
use std::{io::Error, sync::Arc};
use urlencoding::encode;

// Sadly the m3u_parser crate swallows parsing errors completely
// We can't propagate them via api
pub async fn load_playlist() -> AppState {
    let config = AppConfig::load().expect("failed to load Config");
    let url = config.clone().upstream_m3u_url;
    let app_state = AppState {
        config,
        channels: DashMap::new(),
        streams: Arc::new(DashMap::new()),
        client: reqwest::Client::new(),
    };

    let mut playlist = M3uParser::new(None);
    playlist.parse_m3u(&url, false, false).await;
    playlist.get_vector().iter().for_each(|m| {
        // Add stream_info to AppState
        app_state.channels.insert(m.title.clone(), m.clone());
    });

    app_state
}

pub fn deserialize_playlist(state: &AppState) -> Result<bytes::Bytes, Error> {
    let mut out = String::with_capacity(state.channels.len() * 160);
    out.push_str("#EXTM3U\n");

    for entry in state.channels.iter() {
        let info = entry.value();

        // EXTINF line
        out.push_str("#EXTINF:-1 ");

        out.push_str(&format!(
            r#"tvg-id="{}" tvg-name="{}" tvg-logo="{}" group-title="{}","{}""#,
            info.tvg.id, info.tvg.name, info.logo, info.category, info.title,
        ));

        out.push('\n');

        // Proxied URL
        let proxied_url = format!(
            "http://localhost:{}/channel/{}",
            state.config.port,
            encode(&info.title)
        );

        out.push_str(&proxied_url);
        out.push('\n');
    }

    Ok(Bytes::from(out))
}
