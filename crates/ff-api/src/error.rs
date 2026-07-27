use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;
use tracing::warn;

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

        warn!(
            error.kind = kind,
            error.message = %message,
            http.status_code = status.as_u16(),
            "api_error_response"
        );

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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{body, http::StatusCode, response::IntoResponse};
    use serde_json::Value;

    use super::ApiError;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;

        fn make_writer(&'a self) -> SharedBuf {
            self.clone()
        }
    }

    #[tokio::test]
    async fn api_error_response_preserves_body_and_emits_structured_diagnostics() {
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(buf.clone())
            .finish();

        let response = tracing::subscriber::with_default(subscriber, || {
            ApiError::BackendUnavailable("no healthy backend".to_string()).into_response()
        });

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"]["type"], "backend_unavailable");
        assert_eq!(payload["error"]["message"], "no healthy backend");

        let logged = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = logged
            .lines()
            .find(|line| line.contains("api_error_response"))
            .expect("ApiError responses must emit a diagnostic tracing event");
        let event: Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["fields"]["error.kind"], "backend_unavailable");
        assert_eq!(event["fields"]["error.message"], "no healthy backend");
        assert_eq!(event["fields"]["http.status_code"], 503);
    }
}
