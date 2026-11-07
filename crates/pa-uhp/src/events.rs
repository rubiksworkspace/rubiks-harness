//! Translates the `prime-agent -p --mode json` event stream into UHP streaming
//! events and tracks what the task's final response has to report. Pure: lines
//! in, events out, with no sequence numbers (the task assigns those).

use std::collections::{BTreeMap, HashSet};

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Reasoning,
}

/// A streamed block whose `output_item.added` went out and `done` has not.
#[derive(Debug)]
struct OpenBlock {
    kind: BlockKind,
    item_id: String,
    output_index: u64,
    text: String,
}

/// Token counts summed over the task's assistant messages.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Tokens {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

#[derive(Debug, Default)]
pub struct Translator {
    next_output_index: u64,
    /// By the provider's content index within the current assistant message.
    open: BTreeMap<u64, OpenBlock>,
    /// Content indexes of the current message already sent in full.
    finished_blocks: HashSet<u64>,
    finished_call_outputs: HashSet<String>,
    tokens: Tokens,
    steps: u64,
    last_turn_ran_tools: bool,
    model: Option<String>,
    last_stop_reason: Option<String>,
    last_error: Option<String>,
    prime_session_id: Option<String>,
}

impl Translator {
    /// Translate one event of the child's stream.
    pub fn feed(&mut self, event: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        match event.get("type").and_then(Value::as_str) {
            Some("session") => {
                self.prime_session_id = event.get("id").and_then(Value::as_str).map(String::from);
            }
            Some("message_start") if role(event) == Some("assistant") => {
                // A provider retry restarts the message without ending the last.
                self.close_open_blocks(&mut out);
                self.finished_blocks.clear();
            }
            Some("message_update") => {
                if let Some(update) = event.get("assistantMessageEvent") {
                    self.feed_update(update, &mut out);
                }
            }
            Some("message_end") => match role(event) {
                Some("assistant") => self.finish_assistant_message(&event["message"], &mut out),
                Some("toolResult") => {
                    let message = &event["message"];
                    if let Some(call_id) = message.get("toolCallId").and_then(Value::as_str) {
                        self.push_call_output(call_id, &message["content"], &mut out);
                    }
                }
                _ => {}
            },
            Some("tool_execution_end") => {
                if let Some(call_id) = event.get("toolCallId").and_then(Value::as_str) {
                    self.push_call_output(call_id, &event["result"]["content"], &mut out);
                }
            }
            Some("turn_end") => {
                self.steps += 1;
                self.last_turn_ran_tools = event
                    .get("toolResults")
                    .and_then(Value::as_array)
                    .is_some_and(|results| !results.is_empty());
            }
            _ => {}
        }
        out
    }

    /// Close whatever is still open, for a task that stopped mid-message, so
    /// every added item still gets its done.
    pub fn finish(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        self.close_open_blocks(&mut out);
        out
    }

    fn feed_update(&mut self, update: &Value, out: &mut Vec<Value>) {
        let index = update.get("contentIndex").and_then(Value::as_u64);
        let delta = update.get("delta").and_then(Value::as_str);
        match (update.get("type").and_then(Value::as_str), index) {
            (Some("text_delta"), Some(index)) => {
                self.push_delta(index, BlockKind::Text, delta.unwrap_or(""), out);
            }
            (Some("thinking_delta"), Some(index)) => {
                self.push_delta(index, BlockKind::Reasoning, delta.unwrap_or(""), out);
            }
            (Some("text_end"), Some(index)) => {
                self.end_block(index, BlockKind::Text, content_of(update), out);
            }
            (Some("thinking_end"), Some(index)) => {
                self.end_block(index, BlockKind::Reasoning, content_of(update), out);
            }
            (Some("toolcall_end"), Some(index)) if self.finished_blocks.insert(index) => {
                self.push_call(&update["toolCall"], out);
            }
            _ => {}
        }
    }

    /// Opens lazily on the first non-empty fragment, so a block that never had
    /// text (redacted thinking, say) produces no empty item.
    fn push_delta(&mut self, index: u64, kind: BlockKind, delta: &str, out: &mut Vec<Value>) {
        if delta.is_empty() {
            return;
        }
        if !self.open.contains_key(&index) {
            let block = self.open_block(kind, out);
            self.open.insert(index, block);
        }
        let Some(block) = self.open.get_mut(&index) else {
            return;
        };
        block.text.push_str(delta);
        out.push(delta_event(block, delta));
    }

    fn end_block(&mut self, index: u64, kind: BlockKind, content: &str, out: &mut Vec<Value>) {
        if !self.finished_blocks.insert(index) {
            return;
        }
        if let Some(mut block) = self.open.remove(&index) {
            if !content.is_empty() {
                block.text = content.to_string();
            }
            close_block(&block, "completed", out);
        } else if !content.is_empty() {
            // The whole block arrived at once, with no deltas before it.
            let mut block = self.open_block(kind, out);
            block.text.push_str(content);
            out.push(delta_event(&block, content));
            close_block(&block, "completed", out);
        }
    }

    fn open_block(&mut self, kind: BlockKind, out: &mut Vec<Value>) -> OpenBlock {
        let output_index = self.take_output_index();
        let (item_id, item, part_event) = match kind {
            BlockKind::Text => {
                let id = format!("msg_{output_index}");
                let item = json!({
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [],
                });
                let part = json!({
                    "type": "response.content_part.added",
                    "item_id": id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] },
                });
                (id, item, part)
            }
            BlockKind::Reasoning => {
                let id = format!("rs_{output_index}");
                let item = json!({
                    "id": id,
                    "type": "reasoning",
                    "status": "in_progress",
                    "summary": [],
                });
                let part = json!({
                    "type": "response.reasoning_summary_part.added",
                    "item_id": id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "part": { "type": "summary_text", "text": "" },
                });
                (id, item, part)
            }
        };
        out.push(json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        }));
        out.push(part_event);
        OpenBlock {
            kind,
            item_id,
            output_index,
            text: String::new(),
        }
    }

    fn close_open_blocks(&mut self, out: &mut Vec<Value>) {
        for block in std::mem::take(&mut self.open).into_values() {
            close_block(&block, "incomplete", out);
        }
    }

    /// Sent whole once the call is complete: a call's name and id are only
    /// certain at its end, and `output_item.added` must carry them.
    fn push_call(&mut self, call: &Value, out: &mut Vec<Value>) {
        let output_index = self.take_output_index();
        let item_id = format!("fc_{output_index}");
        let call_id = call.get("id").and_then(Value::as_str).unwrap_or("");
        let name = call.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = call
            .get("arguments")
            .map_or_else(|| "{}".to_string(), Value::to_string);
        let item = |status: &str, arguments: &str| {
            json!({
                "id": item_id,
                "type": "function_call",
                "status": status,
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            })
        };
        out.push(json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item("in_progress", ""),
        }));
        out.push(json!({
            "type": "response.function_call_arguments.delta",
            "item_id": item_id,
            "output_index": output_index,
            "delta": arguments,
        }));
        out.push(json!({
            "type": "response.function_call_arguments.done",
            "item_id": item_id,
            "output_index": output_index,
            "arguments": arguments,
        }));
        out.push(json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item("completed", &arguments),
        }));
    }

    fn push_call_output(&mut self, call_id: &str, content: &Value, out: &mut Vec<Value>) {
        if !self.finished_call_outputs.insert(call_id.to_string()) {
            return;
        }
        let output_index = self.take_output_index();
        let text = content
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        let item = json!({
            "id": format!("fco_{output_index}"),
            "type": "function_call_output",
            "call_id": call_id,
            "output": text,
        });
        out.push(json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        }));
        out.push(json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }));
    }

    /// The final message is the truth: closes what was streamed and sends any
    /// block the provider delivered without streaming events.
    fn finish_assistant_message(&mut self, message: &Value, out: &mut Vec<Value>) {
        let blocks = message.get("content").and_then(Value::as_array);
        for (index, block) in (0u64..).zip(blocks.into_iter().flatten()) {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => self.end_block(index, BlockKind::Text, text_of(block, "text"), out),
                Some("thinking") => {
                    self.end_block(index, BlockKind::Reasoning, text_of(block, "thinking"), out);
                }
                Some("toolCall") if self.finished_blocks.insert(index) => {
                    self.push_call(block, out);
                }
                _ => {}
            }
        }
        self.close_open_blocks(out);

        let usage = &message["usage"];
        let count = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
        self.tokens.input += count("input");
        self.tokens.output += count("output");
        self.tokens.cache_read += count("cacheRead");
        self.tokens.cache_write += count("cacheWrite");
        self.model = message
            .get("model")
            .and_then(Value::as_str)
            .map(String::from)
            .or(self.model.take());
        self.last_stop_reason = message
            .get("stopReason")
            .and_then(Value::as_str)
            .map(String::from);
        self.last_error = message
            .get("errorMessage")
            .and_then(Value::as_str)
            .map(String::from);
    }

    fn take_output_index(&mut self) -> u64 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    /// `usage` for the response: null when nothing was reported, because the
    /// spec forbids a zero nobody measured.
    #[must_use]
    pub fn usage_json(&self) -> Value {
        let tokens = self.tokens;
        if tokens == Tokens::default() {
            return Value::Null;
        }
        let input = tokens.input + tokens.cache_read + tokens.cache_write;
        json!({
            "input_tokens": input,
            "output_tokens": tokens.output,
            "total_tokens": input + tokens.output,
            "cache_read_tokens": tokens.cache_read,
            "cache_write_tokens": tokens.cache_write,
        })
    }

    /// Whether a `max_step` budget stops the task: the budget is spent and the
    /// agent would go on. A last turn that ran no tools is the agent finishing,
    /// so that one is left to end on its own.
    #[must_use]
    pub fn step_budget_spent(&self, max_step: u64) -> bool {
        self.steps > max_step || (self.steps == max_step && self.last_turn_ran_tools)
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub fn prime_session_id(&self) -> Option<&str> {
        self.prime_session_id.as_deref()
    }

    /// Whether the last assistant message ended on a provider error.
    #[must_use]
    pub fn ended_on_provider_error(&self) -> bool {
        self.last_stop_reason.as_deref() == Some("error") || self.last_error.is_some()
    }

    /// The provider's own message, for the server log only.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

fn role(event: &Value) -> Option<&str> {
    event.get("message")?.get("role")?.as_str()
}

fn content_of(update: &Value) -> &str {
    text_of(update, "content")
}

fn text_of<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}

fn delta_event(block: &OpenBlock, delta: &str) -> Value {
    match block.kind {
        BlockKind::Text => json!({
            "type": "response.output_text.delta",
            "item_id": block.item_id,
            "output_index": block.output_index,
            "content_index": 0,
            "delta": delta,
        }),
        BlockKind::Reasoning => json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": block.item_id,
            "output_index": block.output_index,
            "summary_index": 0,
            "delta": delta,
        }),
    }
}

fn close_block(block: &OpenBlock, status: &str, out: &mut Vec<Value>) {
    let (id, index) = (&block.item_id, block.output_index);
    let item = match block.kind {
        BlockKind::Text => {
            let part = json!({ "type": "output_text", "text": block.text, "annotations": [] });
            out.push(json!({
                "type": "response.output_text.done",
                "item_id": id,
                "output_index": index,
                "content_index": 0,
                "text": block.text,
            }));
            out.push(json!({
                "type": "response.content_part.done",
                "item_id": id,
                "output_index": index,
                "content_index": 0,
                "part": part,
            }));
            json!({
                "id": id,
                "type": "message",
                "role": "assistant",
                "status": status,
                "content": [part],
            })
        }
        BlockKind::Reasoning => {
            let part = json!({ "type": "summary_text", "text": block.text });
            out.push(json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": id,
                "output_index": index,
                "summary_index": 0,
                "part": part,
            }));
            json!({
                "id": id,
                "type": "reasoning",
                "status": status,
                "summary": [part],
            })
        }
    };
    out.push(json!({
        "type": "response.output_item.done",
        "output_index": index,
        "item": item,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(events: &[Value]) -> Vec<&str> {
        events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect()
    }

    fn update(event: &Value) -> Value {
        json!({ "type": "message_update", "assistantMessageEvent": event })
    }

    fn assistant_end(content: &Value) -> Value {
        json!({ "type": "message_end", "message": {
            "role": "assistant", "content": content, "model": "m-1", "stopReason": "stop",
            "usage": { "input": 10, "output": 5, "cacheRead": 3, "cacheWrite": 2 },
        }})
    }

    #[test]
    fn streamed_text_opens_on_the_first_fragment_and_closes_on_the_end() {
        let mut translator = Translator::default();
        assert!(translator
            .feed(&update(&json!({ "type": "text_start", "contentIndex": 0 })))
            .is_empty());
        let first = translator.feed(&update(
            &json!({ "type": "text_delta", "contentIndex": 0, "delta": "he" }),
        ));
        assert_eq!(
            types(&first),
            [
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta"
            ]
        );
        let second = translator.feed(&update(
            &json!({ "type": "text_delta", "contentIndex": 0, "delta": "y" }),
        ));
        assert_eq!(types(&second), ["response.output_text.delta"]);
        let end = translator.feed(&update(
            &json!({ "type": "text_end", "contentIndex": 0, "content": "hey" }),
        ));
        assert_eq!(
            types(&end),
            [
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done"
            ]
        );
        assert_eq!(end[2]["item"]["content"][0]["text"], "hey");
        assert_eq!(end[2]["item"]["status"], "completed");
        // The final message repeats the block; it must not be sent twice.
        assert!(translator
            .feed(&assistant_end(&json!([{ "type": "text", "text": "hey" }])))
            .is_empty());
    }

    #[test]
    fn a_block_that_was_never_streamed_is_sent_from_the_final_message() {
        let mut translator = Translator::default();
        let events = translator.feed(&assistant_end(&json!([
            { "type": "thinking", "thinking": "hmm" },
            { "type": "text", "text": "done" },
            { "type": "toolCall", "id": "call_1", "name": "bash", "arguments": { "cmd": "ls" } },
        ])));
        let items: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == "response.output_item.done")
            .map(|event| &event["item"])
            .collect();
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items[0]["summary"][0]["text"], "hmm");
        assert_eq!(items[1]["content"][0]["text"], "done");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_1");
        assert_eq!(items[2]["arguments"], r#"{"cmd":"ls"}"#);
    }

    #[test]
    fn a_tool_call_and_its_result_pair_by_call_id_once() {
        let mut translator = Translator::default();
        let call = translator.feed(&update(&json!({
            "type": "toolcall_end", "contentIndex": 0,
            "toolCall": { "id": "call_1", "name": "bash", "arguments": {} },
        })));
        assert_eq!(call.last().unwrap()["item"]["status"], "completed");
        let result = json!({ "content": [{ "type": "text", "text": "out" }] });
        let output = translator.feed(&json!({
            "type": "tool_execution_end", "toolCallId": "call_1", "result": result,
        }));
        assert_eq!(output[1]["item"]["type"], "function_call_output");
        assert_eq!(output[1]["item"]["output"], "out");
        assert_eq!(output[1]["output_index"], 1);
        // The toolResult message repeats the result.
        assert!(translator
            .feed(&json!({ "type": "message_end", "message": {
                "role": "toolResult", "toolCallId": "call_1", "content": [],
            }}))
            .is_empty());
    }

    #[test]
    fn stopping_mid_block_still_closes_the_item_with_what_it_had() {
        let mut translator = Translator::default();
        translator.feed(&update(
            &json!({ "type": "text_delta", "contentIndex": 0, "delta": "par" }),
        ));
        let closing = translator.finish();
        let done = closing.last().unwrap();
        assert_eq!(done["type"], "response.output_item.done");
        assert_eq!(done["item"]["status"], "incomplete");
        assert_eq!(done["item"]["content"][0]["text"], "par");
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn a_retried_message_closes_the_abandoned_block_first() {
        let mut translator = Translator::default();
        translator.feed(&update(
            &json!({ "type": "text_delta", "contentIndex": 0, "delta": "x" }),
        ));
        let restart = translator.feed(&json!({
            "type": "message_start", "message": { "role": "assistant" },
        }));
        assert_eq!(restart.last().unwrap()["item"]["status"], "incomplete");
        let again = translator.feed(&update(
            &json!({ "type": "text_delta", "contentIndex": 0, "delta": "y" }),
        ));
        assert_eq!(again[0]["output_index"], 1);
    }

    #[test]
    fn usage_counts_cache_as_input_and_is_null_when_nothing_was_reported() {
        let mut translator = Translator::default();
        assert!(translator.usage_json().is_null());
        translator.feed(&assistant_end(&json!([])));
        translator.feed(&assistant_end(&json!([])));
        let usage = translator.usage_json();
        assert_eq!(usage["input_tokens"], 30);
        assert_eq!(usage["output_tokens"], 10);
        assert_eq!(usage["total_tokens"], 40);
        assert_eq!(usage["cache_read_tokens"], 6);
        assert_eq!(usage["cache_write_tokens"], 4);
        assert_eq!(translator.model(), Some("m-1"));
    }

    #[test]
    fn the_step_budget_lets_a_finishing_turn_end_on_its_own() {
        let mut translator = Translator::default();
        translator.feed(&json!({ "type": "turn_end", "toolResults": [{}] }));
        assert!(!translator.step_budget_spent(2));
        assert!(translator.step_budget_spent(1));
        translator.feed(&json!({ "type": "turn_end", "toolResults": [] }));
        assert!(!translator.step_budget_spent(2));
        assert!(translator.step_budget_spent(1));
    }

    #[test]
    fn a_provider_error_is_noticed_and_the_session_id_captured() {
        let mut translator = Translator::default();
        translator.feed(&json!({ "type": "session", "id": "abc" }));
        assert_eq!(translator.prime_session_id(), Some("abc"));
        assert!(!translator.ended_on_provider_error());
        translator.feed(&json!({ "type": "message_end", "message": {
            "role": "assistant", "content": [], "stopReason": "error", "errorMessage": "402",
        }}));
        assert!(translator.ended_on_provider_error());
        assert_eq!(translator.last_error(), Some("402"));
    }
}
