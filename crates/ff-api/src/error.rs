use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;

use crate::types::{ErrorBody, ErrorEnvelope};

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("upstream request failed: {0}")]
    Upstream(String),
    #[error("internal server error: {0}")]
    Internal(String),
}

impl ApiError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, kind, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, "bad_request", message),
            Self::BackendUnavailable(message) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "backend_unavailable",
                message,
            ),
            Self::Upstream(message) => (StatusCode::BAD_GATEWAY, "upstream_error", message),
            Self::Internal(message) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
            }
        };

        (
            status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    message,
                    r#type: kind.to_string(),
                },
            }),
        )
            .into_response()
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(error: reqwest::Error) -> Self {
        Self::Upstream(error.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self::BadRequest(error.to_string())
    }
}
