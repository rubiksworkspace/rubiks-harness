//! Wire constants, the error envelope, identifiers and static documents.

use hyper::StatusCode;
use serde_json::{json, Value};

pub const VERSION: &str = "2026-10-19";
pub const VERSION_HEADER: &str = "uhp-version";
pub const HARNESS_ID: &str = "chrn_prime_agent";
const HARNESS_BASE: &str = "prime-agent";
const HARNESS_LABEL: &str = "Prime Agent";

/// A request refused before any task ran: one non-2xx response.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    kind: &'static str,
    pub code: &'static str,
    message: String,
    param: Option<&'static str>,
    /// Boxed: this type is every handler's error, and most have no detail.
    detail: Option<Box<Value>>,
}

impl ApiError {
    fn new(
        status: StatusCode,
        kind: &'static str,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            kind,
            code,
            message: message.into(),
            param: None,
            detail: None,
        }
    }

    pub fn invalid_input(param: &'static str, message: impl Into<String>) -> Self {
        Self {
            param: Some(param),
            ..Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_input",
                message,
            )
        }
    }

    pub fn unsupported_version(requested: &str) -> Self {
        Self {
            detail: Some(Box::new(json!({ "supported": [VERSION] }))),
            ..Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "unsupported_protocol_version",
                format!("This server cannot serve UHP version {requested:?}."),
            )
        }
    }

    pub fn missing_credential() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing_credential",
            "Send a bearer token in the Authorization header.",
        )
    }

    pub fn invalid_credential() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid_credential",
            "The bearer token is not valid.",
        )
    }

    pub fn harness_not_found() -> Self {
        Self::not_found("harness_not_found", "No such harness.")
    }

    pub fn response_not_found() -> Self {
        Self::not_found("response_not_found", "No such response.")
    }

    pub fn session_not_found() -> Self {
        Self::not_found("session_not_found", "No such session.")
    }

    pub fn route_not_found() -> Self {
        Self::not_found("prime_agent_route_not_found", "No such endpoint.")
    }

    fn not_found(code: &'static str, message: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            code,
            message,
        )
    }

    pub fn session_busy() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "invalid_request_error",
            "session_busy",
            "A task is already running in this session.",
        )
    }

    pub fn model_unavailable(message: impl Into<String>) -> Self {
        Self {
            param: Some("model"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request_error",
                "model_unavailable",
                message,
            )
        }
    }

    pub fn body_too_large(max_bytes: usize) -> Self {
        Self {
            detail: Some(Box::new(json!({ "max_bytes": max_bytes }))),
            ..Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "prime_agent_body_too_large",
                "The request body is too large.",
            )
        }
    }

    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "internal_error",
            "The server could not complete the request.",
        )
    }

    #[must_use]
    pub fn envelope(&self) -> Value {
        json!({
            "error": {
                "type": self.kind,
                "code": self.code,
                "message": self.message,
                "param": self.param,
                "detail": self.detail,
            }
        })
    }
}

/// A fresh id with the protocol's type-distinguishing prefix.
#[must_use]
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

/// Whether a client-supplied id can name a stored object: it becomes a file
/// name, so anything but the characters [`new_id`] produces is refused here.
#[must_use]
pub fn is_valid_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

/// `GET /v1/uhp`. Every capability above Core stays false: advertising one this
/// server does not implement is the failure the spec calls out by name.
#[must_use]
pub fn discovery(version: &str) -> Value {
    json!({
        "object": "uhp.discovery",
        "protocol": "uhp",
        "versions": [VERSION],
        "default_version": VERSION,
        "conformance_class": "core",
        "capabilities": {
            "streaming": true,
            "sessions": true,
            "cancellation": true,
            "files_input": false,
            "files_output": false,
            "session_listing": false,
            "harness_management": false,
            "harness_retirement": false,
            "input_history": false,
            "input_requests": false,
            "session_sharing": false,
            "idempotency": false,
            "harness_idempotency": false,
            "plugins": false,
            "environments": false,
            "memories": false,
        },
        "implementation": { "name": "Prime Agent (Rubiks)", "version": version },
    })
}

/// The one harness this server runs.
#[must_use]
pub fn harness(created_at_ms: u64, default_model: Option<&str>) -> Value {
    let mut harness = json!({
        "id": HARNESS_ID,
        "object": "harness",
        "name": HARNESS_LABEL,
        "base": HARNESS_BASE,
        "baseLabel": HARNESS_LABEL,
        "systemPrompt": "",
        "mcpServers": [],
        "skills": [],
        "plugins": [],
        "disabledTools": [],
        "createdAt": created_at_ms,
    });
    if let Some(model) = default_model {
        harness["defaultModel"] = json!(model);
    }
    harness
}

/// `GET /v1/harnesses/{id}/models`.
#[must_use]
pub fn harness_models(default_model: Option<&str>, models: &[Value]) -> Value {
    let mut body = json!({
        "harness_id": HARNESS_ID,
        "backend": HARNESS_BASE,
        "models": models,
    });
    if let Some(model) = default_model {
        body["default"] = json!(model);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_ids_cannot_escape_the_state_directory() {
        assert!(is_valid_id(&new_id("resp_"), "resp_"));
        assert!(!is_valid_id("resp_", "resp_"));
        assert!(!is_valid_id("resp_..", "resp_"));
        assert!(!is_valid_id("resp_a%2Fb", "resp_"));
        assert!(!is_valid_id("hsessabc", "resp_"));
    }

    #[test]
    fn the_envelope_always_has_every_field() {
        let envelope = ApiError::harness_not_found().envelope();
        assert_eq!(envelope["error"]["code"], "harness_not_found");
        assert!(envelope["error"]["param"].is_null());
        assert!(envelope["error"]["detail"].is_null());
    }
}
