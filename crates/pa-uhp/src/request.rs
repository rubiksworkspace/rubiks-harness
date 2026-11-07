//! Parses the body of `POST /v1/responses` into what a task needs.

use serde_json::{json, Map, Value};

use crate::protocol::ApiError;

/// Fields this server acts on. Anything else a request carries is accepted and
/// named in `metadata.ignored_fields`, so dropping it stays observable.
const ACTED_ON_FIELDS: [&str; 10] = [
    "input",
    "model",
    "metadata",
    "stream",
    "previous_response_id",
    "instructions",
    "store",
    "max_step",
    "timeout_seconds",
    "background",
];

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRequest {
    /// The last user message: the task's own message.
    pub prompt: String,
    pub instructions: Option<String>,
    pub input_items: Vec<Value>,
    pub model: Option<String>,
    pub harness_id: Option<String>,
    pub previous_response_id: Option<String>,
    /// The client's metadata, echoed back in the response's.
    pub metadata: Map<String, Value>,
    pub store: bool,
    pub stream: bool,
    pub background: bool,
    pub max_step: Option<u64>,
    pub timeout_seconds: Option<u64>,
    pub ignored_fields: Vec<String>,
}

fn optional_string(
    body: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, ApiError> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ApiError::invalid_input(
            field,
            format!("{field} must be a string."),
        )),
    }
}

fn optional_bool(body: &Map<String, Value>, field: &'static str) -> Result<Option<bool>, ApiError> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ApiError::invalid_input(
            field,
            format!("{field} must be a boolean."),
        )),
    }
}

fn optional_budget(
    body: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<u64>, ApiError> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|budget| *budget > 0)
            .map(Some)
            .ok_or_else(|| {
                ApiError::invalid_input(field, format!("{field} must be a positive integer."))
            }),
    }
}

/// The text of a message's `content`, refusing parts this server has no
/// capability for rather than silently running without them.
fn message_text(content: &Value) -> Result<String, ApiError> {
    match content {
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut texts = Vec::with_capacity(parts.len());
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                            ApiError::invalid_input("input", "A text part must carry text.")
                        })?;
                        texts.push(text);
                    }
                    Some("input_file" | "input_image") => {
                        return Err(ApiError::invalid_input(
                            "input",
                            "This server does not accept file input.",
                        ));
                    }
                    _ => {
                        return Err(ApiError::invalid_input(
                            "input",
                            "A message holds an unknown content part type.",
                        ));
                    }
                }
            }
            Ok(texts.join("\n"))
        }
        _ => Err(ApiError::invalid_input(
            "input",
            "A message's content must be a string or an array of parts.",
        )),
    }
}

/// The prompt, instructions from `system`/`developer` messages, and the items
/// as read back through `input_items`. Earlier messages are not given to the
/// harness: `input_history` is false, which the spec permits.
fn parse_input(input: Option<&Value>) -> Result<(String, Vec<String>, Vec<Value>), ApiError> {
    let items = match input {
        Some(Value::String(text)) => vec![json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        })],
        Some(Value::Array(items)) => items.clone(),
        _ => {
            return Err(ApiError::invalid_input(
                "input",
                "input must be a string or an array of items.",
            ));
        }
    };
    let mut prompt = None;
    let mut instructions = Vec::new();
    for item in &items {
        match item.get("type").and_then(Value::as_str) {
            Some("input_response") => {
                return Err(ApiError::invalid_input(
                    "input",
                    "This server does not ask for input, so there is nothing to answer.",
                ));
            }
            None | Some("message") => {}
            Some(_) => continue,
        }
        let content = item.get("content").unwrap_or(&Value::Null);
        match item.get("role").and_then(Value::as_str) {
            Some("user") => prompt = Some(message_text(content)?),
            Some("system" | "developer") => instructions.push(message_text(content)?),
            Some("assistant") => {}
            _ => {
                return Err(ApiError::invalid_input(
                    "input",
                    "A message must have a role.",
                ));
            }
        }
    }
    let prompt = prompt
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| ApiError::invalid_input("input", "input has no user message."))?;
    Ok((prompt, instructions, items))
}

/// # Errors
/// An [`ApiError`] for a body that is not a valid task request.
pub fn parse(body: &[u8]) -> Result<TaskRequest, ApiError> {
    let body: Value = serde_json::from_slice(body)
        .map_err(|_| ApiError::invalid_input("input", "The body is not valid JSON."))?;
    let Value::Object(body) = body else {
        return Err(ApiError::invalid_input(
            "input",
            "The body must be a JSON object.",
        ));
    };
    let (prompt, mut instructions, input_items) = parse_input(body.get("input"))?;
    if let Some(field) = optional_string(&body, "instructions")? {
        instructions.insert(0, field);
    }
    let metadata = match body.get("metadata") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(metadata)) => metadata.clone(),
        Some(_) => {
            return Err(ApiError::invalid_input(
                "metadata",
                "metadata must be an object.",
            ));
        }
    };
    let harness_id = match metadata.get("harness_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.clone()),
        Some(_) => {
            return Err(ApiError::invalid_input(
                "metadata",
                "metadata.harness_id must be a string.",
            ));
        }
    };
    let ignored_fields = body
        .keys()
        .filter(|field| !ACTED_ON_FIELDS.contains(&field.as_str()))
        .cloned()
        .collect();
    Ok(TaskRequest {
        prompt,
        instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
        input_items,
        model: optional_string(&body, "model")?,
        harness_id,
        previous_response_id: optional_string(&body, "previous_response_id")?,
        metadata,
        store: optional_bool(&body, "store")?.unwrap_or(true),
        stream: optional_bool(&body, "stream")?.unwrap_or(false),
        background: optional_bool(&body, "background")?.unwrap_or(false),
        max_step: optional_budget(&body, "max_step")?,
        timeout_seconds: optional_budget(&body, "timeout_seconds")?,
        ignored_fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(body: &Value) -> Result<TaskRequest, ApiError> {
        parse(body.to_string().as_bytes())
    }

    #[test]
    fn a_bare_string_is_one_user_message() {
        let request = parsed(&json!({ "input": "hello" })).unwrap();
        assert_eq!(request.prompt, "hello");
        assert_eq!(request.input_items[0]["role"], "user");
        assert!(request.store);
        assert!(request.ignored_fields.is_empty());
    }

    #[test]
    fn the_last_user_message_is_the_task_and_system_messages_are_instructions() {
        let request = parsed(&json!({
            "instructions": "be brief",
            "input": [
                { "role": "user", "content": "first" },
                { "role": "assistant", "content": "ok" },
                { "role": "developer", "content": [{ "type": "input_text", "text": "use tabs" }] },
                { "type": "message", "role": "user",
                  "content": [{ "type": "input_text", "text": "second" }] },
            ],
        }))
        .unwrap();
        assert_eq!(request.prompt, "second");
        assert_eq!(
            request.instructions.as_deref(),
            Some("be brief\n\nuse tabs")
        );
    }

    #[test]
    fn only_fields_actually_sent_are_reported_as_ignored() {
        let request =
            parsed(&json!({ "input": "x", "tools": [], "include": [], "future_field": 1 }))
                .unwrap();
        assert_eq!(request.ignored_fields, ["tools", "include", "future_field"]);
    }

    #[test]
    fn refusals_name_invalid_input() {
        for body in [
            json!({}),
            json!({ "input": 5 }),
            json!({ "input": "  " }),
            json!({ "input": [{ "role": "assistant", "content": "only me" }] }),
            json!({ "input": [{ "role": "user", "content": [{ "type": "input_file", "file_id": "f" }] }] }),
            json!({ "input": [{ "type": "input_response", "request_id": "r" }] }),
            json!({ "input": "x", "stream": "yes" }),
            json!({ "input": "x", "max_step": 0 }),
            json!({ "input": "x", "metadata": { "harness_id": 3 } }),
        ] {
            assert_eq!(parsed(&body).unwrap_err().code, "invalid_input", "{body}");
        }
    }

    #[test]
    fn a_body_that_is_not_json_is_refused() {
        assert_eq!(parse(b"nope").unwrap_err().code, "invalid_input");
    }
}
