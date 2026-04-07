//! JSON-RPC 2.0 protocol types for MCP.
//!
//! Follows the JSON-RPC 2.0 specification: <https://www.jsonrpc.org/specification>

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── JSON-RPC 2.0 Request ────────────────────────────────────────────────────

/// A JSON-RPC 2.0 request object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Must be "2.0".
    pub jsonrpc: String,

    /// Method name to invoke.
    pub method: String,

    /// Parameters (positional or named). `None` if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,

    /// Request identifier. `None` for notifications (no response expected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

// ─── JSON-RPC 2.0 Response ──────────────────────────────────────────────────

/// A JSON-RPC 2.0 response object.
///
/// Exactly one of `result` or `error` must be present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Must be "2.0".
    pub jsonrpc: String,

    /// Successful result. Present on success, absent on error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,

    /// Error object. Present on failure, absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,

    /// Request identifier (echoed from the request).
    pub id: Option<Value>,
}

impl JsonRpcResponse {
    /// Build a success response.
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

    /// Build an error response.
    pub fn error(id: Option<Value>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(error),
            id,
        }
    }
}

// ─── JSON-RPC 2.0 Error ─────────────────────────────────────────────────────

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code.
    pub code: i64,

    /// Human-readable error message.
    pub message: String,

    /// Optional additional data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ─── Standard error codes ────────────────────────────────────────────────────

/// Standard JSON-RPC 2.0 error codes.
pub mod error_codes {
    /// Invalid JSON was received by the server.
    pub const PARSE_ERROR: i64 = -32700;

    /// The JSON sent is not a valid Request object.
    pub const INVALID_REQUEST: i64 = -32600;

    /// The method does not exist / is not available.
    pub const METHOD_NOT_FOUND: i64 = -32601;

    /// Invalid method parameter(s).
    pub const INVALID_PARAMS: i64 = -32602;

    /// Internal JSON-RPC error.
    pub const INTERNAL_ERROR: i64 = -32603;
}

impl JsonRpcError {
    pub fn parse_error(detail: impl Into<String>) -> Self {
        Self {
            code: error_codes::PARSE_ERROR,
            message: "Parse error".to_string(),
            data: Some(Value::String(detail.into())),
        }
    }

    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self {
            code: error_codes::INVALID_REQUEST,
            message: "Invalid Request".to_string(),
            data: Some(Value::String(detail.into())),
        }
    }

    pub fn method_not_found(method: impl Into<String>) -> Self {
        Self {
            code: error_codes::METHOD_NOT_FOUND,
            message: "Method not found".to_string(),
            data: Some(Value::String(method.into())),
        }
    }

    pub fn invalid_params(detail: impl Into<String>) -> Self {
        Self {
            code: error_codes::INVALID_PARAMS,
            message: "Invalid params".to_string(),
            data: Some(Value::String(detail.into())),
        }
    }

    pub fn internal_error(detail: impl Into<String>) -> Self {
        Self {
            code: error_codes::INTERNAL_ERROR,
            message: "Internal error".to_string(),
            data: Some(Value::String(detail.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_request() {
        let json = r#"{"jsonrpc":"2.0","method":"fleet_status","params":{},"id":1}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "fleet_status");
        assert_eq!(req.id, Some(Value::Number(1.into())));
    }

    #[test]
    fn serialize_success_response() {
        let resp = JsonRpcResponse::success(Some(Value::Number(1.into())), Value::Bool(true));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"result\":true"));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn serialize_error_response() {
        let err = JsonRpcError::method_not_found("unknown_method");
        let resp = JsonRpcResponse::error(Some(Value::Number(1.into())), err);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("-32601"));
        assert!(!json.contains("\"result\""));
    }
}
