//! JSON-RPC 2.0 message types for ACP protocol.
//!
//! ACP (Agent Client Protocol) uses JSON-RPC 2.0 over stdio for communication
//! with AI coding agents.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type RequestId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Uuid::new_v4().to_string(),
            method: method.into(),
            params,
        }
    }

    pub fn new_with_id(
        id: impl Into<String>,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    #[allow(dead_code)]
    pub fn error(id: RequestId, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn parse_error() -> Self {
        Self::new(-32700, "Parse error")
    }

    #[allow(dead_code)]
    pub fn invalid_request() -> Self {
        Self::new(-32600, "Invalid request")
    }

    #[allow(dead_code)]
    pub fn method_not_found() -> Self {
        Self::new(-32601, "Method not found")
    }

    #[allow(dead_code)]
    pub fn invalid_params() -> Self {
        Self::new(-32602, "Invalid params")
    }

    pub fn internal_error() -> Self {
        Self::new(-32603, "Internal error")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params,
        }
    }
}

pub fn parse_message(data: &str) -> Result<JsonRpcMessage, JsonRpcError> {
    serde_json::from_str(data).map_err(|_| JsonRpcError::parse_error())
}

pub fn serialize_request(request: &JsonRpcRequest) -> Result<String, JsonRpcError> {
    serde_json::to_string(request).map_err(|_| JsonRpcError::internal_error())
}

pub fn serialize_notification(notification: &JsonRpcNotification) -> Result<String, JsonRpcError> {
    serde_json::to_string(notification).map_err(|_| JsonRpcError::internal_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_request_serialization() {
        let request = JsonRpcRequest::new("initialize", Some(json!({"cwd": "/tmp"})));
        let json = serialize_request(&request).unwrap();
        assert!(json.contains("initialize"));
        assert!(json.contains("2.0"));
    }

    #[test]
    fn test_response_success() {
        let response = JsonRpcResponse::success("id-123".to_string(), json!({"status": "ok"}));
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"result\""));
        assert!(json.contains("ok"));
    }

    #[test]
    fn test_response_error() {
        let response =
            JsonRpcResponse::error("id-456".to_string(), JsonRpcError::method_not_found());
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"error\""));
        assert!(json.contains("-32601"));
    }

    #[test]
    fn test_notification() {
        let notification =
            JsonRpcNotification::new("session/update", Some(json!({"status": "running"})));
        let json = serialize_notification(&notification).unwrap();
        assert!(json.contains("session/update"));
    }

    #[test]
    fn test_parse_message_request() {
        let json = r#"{"jsonrpc":"2.0","id":"abc","method":"test","params":null}"#;
        let message = parse_message(json).unwrap();
        assert!(matches!(message, JsonRpcMessage::Request(_)));
    }

    #[test]
    fn test_parse_malformed() {
        let json = r#"{"invalid": json}"#;
        let result = parse_message(json);
        assert!(result.is_err());
    }
}
