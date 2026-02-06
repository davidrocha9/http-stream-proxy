use axum::http::StatusCode;
use axum::response::IntoResponse;
use bytes::Bytes;
use thiserror::Error;
use tokio::sync::broadcast;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("channel not found: {title}")]
    ChannelNotFound { title: String },

    #[error("failed to start upstream request")]
    UpstreamRequest {
        #[source]
        source: reqwest::Error,
    },

    #[error("upstream stream error")]
    UpstreamStream {
        #[source]
        source: reqwest::Error,
    },

    #[error("failed to start transcoder")]
    TranscoderStart {
        #[source]
        source: std::io::Error,
    },

    #[error("broadcast channel error")]
    DownstreamSend {
        #[source]
        source: broadcast::error::SendError<Bytes>,
    },

    #[error("no active stream")]
    GuestError,
}

impl ProxyError {
    fn status_code(&self) -> StatusCode {
        match self {
            ProxyError::ChannelNotFound { .. } => StatusCode::NOT_FOUND,
            ProxyError::UpstreamRequest { .. } => StatusCode::BAD_GATEWAY,
            ProxyError::UpstreamStream { .. } => StatusCode::BAD_GATEWAY,
            ProxyError::TranscoderStart { .. } => StatusCode::BAD_GATEWAY,
            ProxyError::DownstreamSend { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            ProxyError::GuestError => StatusCode::NOT_FOUND,
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> axum::response::Response {
        let status = self.status_code();

        tracing::error!(error = %self);

        status.into_response()
    }
}

#[derive(Debug, Error)]
pub enum UpstreamFetchError {
    #[error("failed to request m3u playlist")]
    UpstreamRequest {
        #[source]
        source: reqwest::Error,
    },

    #[error("failed to read m3u body")]
    BodyRead {
        #[source]
        source: reqwest::Error,
    },

    #[error("failed to construct m3u response")]
    ResponseBuild {
        #[source]
        source: axum::http::Error,
    },
}
impl UpstreamFetchError {
    fn status_code(&self) -> StatusCode {
        match self {
            UpstreamFetchError::UpstreamRequest { .. } => StatusCode::BAD_GATEWAY,
            UpstreamFetchError::BodyRead { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            UpstreamFetchError::ResponseBuild { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for UpstreamFetchError {
    fn into_response(self) -> axum::response::Response {
        let status = self.status_code();

        tracing::error!(error = %self);

        status.into_response()
    }
}
