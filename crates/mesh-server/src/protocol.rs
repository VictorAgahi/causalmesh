use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestMeta {
    /// W3C Trace Context (e.g. "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
    pub traceparent: Option<String>,
    pub tracestate: Option<String>,
}

impl RequestMeta {
    /// Extracts the 32-character hex trace_id from a W3C traceparent header
    pub fn extract_trace_id(&self) -> Option<String> {
        let tp = self.traceparent.as_ref()?;
        let parts: Vec<&str> = tp.split('-').collect();
        if parts.len() >= 4 && parts[1].len() == 32 {
            Some(parts[1].to_string())
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Must be exactly `"2.0"`; checked by [`classify`] (absent → Invalid Request).
    #[serde(default)]
    pub jsonrpc: Option<String>,
    /// `None` only when the member is absent — a notification. An explicit
    /// `"id": null` is `Some(Value::Null)`: still a request that gets an answer.
    #[serde(default, deserialize_with = "present_value")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

fn present_value<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// One incoming line, sorted by what JSON-RPC 2.0 says the server owes it.
#[derive(Debug)]
pub enum Incoming {
    /// A request with an `id`: dispatch it and answer.
    Request(JsonRpcRequest),
    /// No `id` member: a notification. The server must never reply to it —
    /// not even with an error (JSON-RPC 2.0 §4.1).
    Notification(JsonRpcRequest),
    /// Not a valid request at all: send this error response as-is.
    Reject(JsonRpcResponse),
}

/// Classifies one newline-framed message:
/// - not JSON → `-32700` Parse error (`id: null`, as §5 requires when the id
///   cannot be read);
/// - JSON but not a request object, or `jsonrpc` other than `"2.0"` →
///   `-32600` Invalid Request, echoing the `id` when one could be read;
/// - no `id` member → [`Incoming::Notification`].
pub fn classify(line: &str) -> Incoming {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Incoming::Reject(JsonRpcResponse::error(
                Some(Value::Null),
                -32700,
                format!("Parse error: {e}"),
                None,
            ))
        }
    };
    let echoed_id = || value.get("id").cloned().or(Some(Value::Null));
    let req: JsonRpcRequest = match serde_json::from_value(value.clone()) {
        Ok(r) => r,
        Err(e) => {
            return Incoming::Reject(JsonRpcResponse::error(
                echoed_id(),
                -32600,
                format!("Invalid Request: {e}"),
                None,
            ))
        }
    };
    if req.jsonrpc.as_deref() != Some("2.0") {
        return Incoming::Reject(JsonRpcResponse::error(
            echoed_id(),
            -32600,
            "Invalid Request: \"jsonrpc\" must be \"2.0\"",
            None,
        ));
    }
    if req.id.is_none() {
        Incoming::Notification(req)
    } else {
        Incoming::Request(req)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always "2.0"; a static str avoids one heap allocation per response.
    pub jsonrpc: std::borrow::Cow<'static, str>,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: std::borrow::Cow::Borrowed("2.0"),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(
        id: Option<Value>,
        code: i32,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self {
            jsonrpc: std::borrow::Cow::Borrowed("2.0"),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_follows_json_rpc_2_0() {
        assert!(matches!(
            classify(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
            Incoming::Request(_)
        ));
        // Explicit null id is still a request.
        assert!(matches!(
            classify(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#),
            Incoming::Request(r) if r.id == Some(Value::Null)
        ));
        assert!(matches!(
            classify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Incoming::Notification(_)
        ));
        let code = |line: &str| match classify(line) {
            Incoming::Reject(r) => r.error.map(|e| e.code),
            _ => None,
        };
        assert_eq!(code("{not json"), Some(-32700));
        assert_eq!(code(r#"{"id":3,"method":"ping"}"#), Some(-32600));
        assert_eq!(
            code(r#"{"jsonrpc":"1.0","id":3,"method":"ping"}"#),
            Some(-32600)
        );
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":3}"#), Some(-32600));
        assert_eq!(code("[1,2]"), Some(-32600));
        // The id is echoed on Invalid Request when it could be read.
        assert!(matches!(
            classify(r#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#),
            Incoming::Reject(r) if r.id == Some(serde_json::json!(7))
        ));
    }

    #[test]
    fn test_w3c_traceparent_extraction() {
        let meta = RequestMeta {
            traceparent: Some(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
            ),
            tracestate: None,
        };

        assert_eq!(
            meta.extract_trace_id(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736".to_string())
        );
    }
}
