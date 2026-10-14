//! The capture points. Model calls go through [`instrument_stream_fn`]
//! (`context.built`, `api.call`); agent events through
//! [`observe_agent_events`] (`user.prompt`, `tool.result`, `verification`);
//! compaction and retries report through [`Recorder::note_compaction`] and
//! [`Recorder::note_auto_retry`]. Errors and their corrections are paired
//! here:
//!
//! - An error is detected when a tool result is an error or exits non-zero
//!   (`tool_error`), a model call fails (`provider_error`), or the same tool
//!   call with the same arguments runs three times in a row (`loop`).
//! - A correction answers an open error: the error output reaching the model
//!   in the next call (`error_message`, for tool errors), an automatic
//!   provider retry (`retry`, for provider errors), or the user's next
//!   message (`user_message`, for any error still open).
//!
//! Each agent run ends with `run.end`: the final diff against the commit the
//! run started on, whether it was committed, the checks it ran, its cost,
//! model calls and wall time.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use fancy_regex::Regex;
use pa_agent::stream::{
    AssistantMessageEvent, LlmContext, ModelStream, OnPayloadHook, OnProviderDetailHook,
    OnResponseHook, StreamFn, StreamRequestOptions,
};
use pa_agent::types::{
    AgentEvent, AgentMessage, AgentToolResult, AssistantContent, AssistantMessage, Message,
    StopReason, ToolResultContent, UserContent, UserPart,
};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use super::store::hex;
use super::Recorder;
use crate::session_engine::auto_retry::{AutoRetryEvent, RetryStartReason};

/// Repeats of the same tool call that count as a loop.
const LOOP_REPEATS: u32 = 3;
/// Characters of a verification command kept in its record.
const COMMAND_CHARS: usize = 300;
/// The final diff kept in `run.end`, at most.
const DIFF_BYTES: usize = 2 * 1024 * 1024;

/// The decision state between records.
#[derive(Default)]
pub(super) struct Decisions {
    errors_detected: u64,
    open_errors: Vec<OpenError>,
    /// The latest tool call's name and argument hash, and how many times in
    /// a row it ran.
    last_call: Option<(String, u32)>,
    /// Running test, lint and type-check commands by tool call id.
    running_checks: HashMap<String, (&'static str, String)>,
    /// The agent run in progress (from `AgentStart` to `AgentEnd`).
    run: Option<RunProgress>,
}

struct RunProgress {
    started: Instant,
    first_step: u64,
    cost: f64,
    errors: u64,
    /// Each check's kind and whether it passed, in order.
    checks: Vec<(&'static str, bool)>,
}

struct OpenError {
    id: String,
    kind: &'static str,
    step: u64,
}

impl Recorder {
    fn decisions(&self) -> std::sync::MutexGuard<'_, Decisions> {
        self.decisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn error_detected(&self, kind: &'static str, mut data: Value) {
        let step = self.current_step();
        let id = {
            let mut decisions = self.decisions();
            decisions.errors_detected += 1;
            if let Some(run) = decisions.run.as_mut() {
                run.errors += 1;
            }
            let id = format!("e{}", decisions.errors_detected);
            decisions.open_errors.push(OpenError {
                id: id.clone(),
                kind,
                step,
            });
            id
        };
        data["id"] = json!(id);
        data["type"] = json!(kind);
        self.record("error.detected", Some(step), data, Vec::new());
    }

    /// Answer the open errors `answers` selects with a correction of `form`.
    fn deliver_corrections(&self, form: &'static str, answers: impl Fn(&str) -> bool) {
        let step = self.current_step();
        let answered: Vec<OpenError> = {
            let mut decisions = self.decisions();
            let (answered, open) = std::mem::take(&mut decisions.open_errors)
                .into_iter()
                .partition(|error| answers(error.kind));
            decisions.open_errors = open;
            answered
        };
        for error in answered {
            self.record(
                "correction.delivered",
                Some(step),
                json!({
                    "answers": error.id,
                    "errorType": error.kind,
                    "form": form,
                    "errorStep": error.step,
                    "lagSteps": step.saturating_sub(error.step),
                }),
                Vec::new(),
            );
        }
    }

    /// `context.built`: what the model is about to see.
    fn context_built(&self, step: u64, context: &LlmContext) {
        let mut roles: HashMap<&'static str, usize> = HashMap::new();
        let mut images = 0usize;
        let mut message_bytes = 0usize;
        let mut digest = false;
        let mut tool_results = 0usize;
        for message in &context.messages {
            message_bytes += serde_json::to_string(message).map_or(0, |text| text.len());
            let role = match message {
                Message::User(user) => {
                    let text = user_text(&user.content);
                    digest |= text.starts_with("[harness-digest]");
                    images += user_images(&user.content);
                    "user"
                }
                Message::Assistant(_) => "assistant",
                Message::ToolResult(result) => {
                    tool_results += 1;
                    images += result
                        .content
                        .iter()
                        .filter(|part| matches!(part, ToolResultContent::Image(_)))
                        .count();
                    "toolResult"
                }
            };
            *roles.entry(role).or_default() += 1;
        }
        let system_bytes = context.system_prompt.as_ref().map_or(0, String::len);
        self.record(
            "context.built",
            Some(step),
            json!({
                "messages": context.messages.len(),
                "byRole": roles,
                "toolResults": tool_results,
                "images": images,
                "harnessDigest": digest,
                "systemPromptBytes": system_bytes,
                "systemPromptSha256": context.system_prompt.as_deref().map(sha256),
                "tools": context.tools.len(),
                "messageBytes": message_bytes,
                // chars/4, the harness's own estimate before the provider counts.
                "estimatedTokens": (system_bytes + message_bytes) / 4,
            }),
            Vec::new(),
        );
        // The previous call's tool errors reach the model in this one.
        self.deliver_corrections("error_message", |kind| kind == "tool_error");
    }

    /// A retry event from the provider retry loop (`api.retry`).
    pub fn note_auto_retry(&self, event: &AutoRetryEvent) {
        let data = match event {
            AutoRetryEvent::Start {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
                reason,
            } => json!({
                "phase": "start",
                "attempt": attempt,
                "maxAttempts": max_attempts,
                "delayMs": delay_ms,
                "error": error_message,
                "backupModel": match reason {
                    RetryStartReason::Quick => None,
                    RetryStartReason::Backup { backup_model } => Some(backup_model),
                },
            }),
            AutoRetryEvent::End {
                success,
                attempt,
                final_error,
                restored_model,
            } => json!({
                "phase": "end",
                "success": success,
                "attempt": attempt,
                "finalError": final_error,
                "restoredModel": restored_model,
            }),
        };
        self.record("api.retry", None, data, Vec::new());
        if matches!(event, AutoRetryEvent::Start { .. }) {
            self.deliver_corrections("retry", |kind| kind == "provider_error");
        }
    }

    /// `context.compacted`: a compaction committed. `messages_summarized`
    /// left the live context for `summary`.
    pub fn note_compaction(
        &self,
        tokens_before: u64,
        tokens_after: u64,
        messages_summarized: usize,
        first_kept_entry_id: &str,
        summary: &str,
    ) {
        self.record(
            "context.compacted",
            None,
            json!({
                "tokensBefore": tokens_before,
                "tokensAfter": tokens_after,
                "messagesSummarized": messages_summarized,
                "firstKeptEntryId": first_kept_entry_id,
                "summaryChars": summary.chars().count(),
            }),
            vec![("summary", summary.to_string())],
        );
    }

    pub(super) fn observe(&self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageStart {
                message: AgentMessage::Standard(Message::User(user)),
            } => {
                let text = user_text(&user.content);
                self.record(
                    "user.prompt",
                    None,
                    json!({
                        "chars": text.chars().count(),
                        "images": user_images(&user.content),
                    }),
                    vec![("text", text)],
                );
                self.deliver_corrections("user_message", |_| true);
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => self.tool_started(tool_call_id, tool_name, args),
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => self.tool_finished(tool_call_id, tool_name, result, *is_error),
            AgentEvent::AgentStart => {
                let first_step = self.current_step() + 1;
                self.decisions().run = Some(RunProgress {
                    started: Instant::now(),
                    first_step,
                    cost: 0.0,
                    errors: 0,
                    checks: Vec::new(),
                });
            }
            AgentEvent::AgentEnd { messages } => self.run_ended(messages),
            AgentEvent::TurnStart
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::ToolExecutionUpdate { .. } => {}
        }
    }

    /// `run.end`. Asking git happens on its own thread: the agent only
    /// becomes idle after its listeners return.
    fn run_ended(self: &Recorder, messages: &[AgentMessage]) {
        let (run, open_errors) = {
            let mut decisions = self.decisions();
            (decisions.run.take(), decisions.open_errors.len())
        };
        let Some(run) = run else {
            return;
        };
        let stop_reason = messages.iter().rev().find_map(|message| match message {
            AgentMessage::Standard(Message::Assistant(assistant)) => {
                Some(format!("{:?}", assistant.stop_reason).to_lowercase())
            }
            AgentMessage::Standard(_) | AgentMessage::Custom(_) => None,
        });
        let last_step = self.current_step();
        let mut data = json!({
            "durationMs": u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "modelCalls": (last_step + 1).saturating_sub(run.first_step),
            "firstStep": run.first_step,
            "cost": run.cost,
            "errorsDetected": run.errors,
            "errorsOpen": open_errors,
            "checks": run.checks.iter().map(|(kind, passed)| json!({ "kind": kind, "passed": passed })).collect::<Vec<_>>(),
            "lastCheckPassed": run.checks.last().map(|(_, passed)| *passed),
            "stopReason": stop_reason,
        });
        let start_commit = self
            .start_commit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let cwd = self.cwd.clone();
        let Some(recorder) = super::recorder_for(&self.session_id) else {
            self.record("run.end", Some(last_step), data, Vec::new());
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("research-run-end".to_string())
            .spawn(move || {
                let mut content = Vec::new();
                if let Some(start) = start_commit {
                    let git = |args: &[&str]| super::run_info::git(&cwd, args);
                    let end_commit = git(&["rev-parse", "HEAD"]);
                    let changed = git(&["status", "--porcelain", "--untracked-files=all"])
                        .map_or(0, |status| status.lines().count());
                    data["repo"] = json!({
                        "startCommit": start,
                        "endCommit": end_commit,
                        "committed": end_commit.as_deref().is_some_and(|end| end != start),
                        "uncommittedFiles": changed,
                        "diffStat": git(&["diff", "--shortstat", &start]),
                    });
                    if let Some(mut diff) = git(&["diff", &start]) {
                        if diff.len() > DIFF_BYTES {
                            diff.truncate(diff.floor_char_boundary(DIFF_BYTES));
                            data["repo"]["diffTruncated"] = json!(true);
                        }
                        content.push(("diff", diff));
                    }
                }
                recorder.record("run.end", Some(last_step), data, content);
            });
        if let Err(error) = spawned {
            tracing::debug!(%error, "research run.end not recorded");
        }
    }

    fn tool_started(&self, tool_call_id: &str, tool_name: &str, args: &Value) {
        let key = format!(
            "{tool_name}:{}",
            sha256(&serde_json::to_string(args).unwrap_or_default())
        );
        let repeats = {
            let mut decisions = self.decisions();
            let repeats = match &decisions.last_call {
                Some((last, count)) if *last == key => count + 1,
                _ => 1,
            };
            decisions.last_call = Some((key, repeats));
            if let Some(check) = command_of(args).and_then(|command| {
                verification_kind(&command).map(|kind| (kind, truncate(&command, COMMAND_CHARS)))
            }) {
                decisions
                    .running_checks
                    .insert(tool_call_id.to_string(), check);
            }
            repeats
        };
        if repeats == LOOP_REPEATS {
            self.error_detected(
                "loop",
                json!({ "tool": tool_name, "toolCallId": tool_call_id, "repeats": repeats }),
            );
        }
    }

    fn tool_finished(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        result: &AgentToolResult,
        is_error: bool,
    ) {
        let shown: String = result
            .content
            .iter()
            .filter_map(|part| match part {
                ToolResultContent::Text(text) => Some(text.text.as_str()),
                ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let details = &result.details;
        let truncation = &details["truncation"];
        // The output before the harness shaped it: the bash tool reports its
        // total size when it truncated; the Python tool keeps both streams.
        let raw: Option<String> = match (details["stdout"].as_str(), details["stderr"].as_str()) {
            (None, None) => None,
            (stdout, stderr) => Some(format!(
                "{}{}",
                stdout.unwrap_or_default(),
                stderr.unwrap_or_default()
            )),
        };
        let raw_bytes = truncation["totalBytes"]
            .as_u64()
            .or_else(|| raw.as_ref().map(|raw| raw.len() as u64))
            .unwrap_or(shown.len() as u64);
        let exit_code = exit_code(&shown);
        let failed = is_error || exit_code.is_some_and(|code| code != 0);
        let mut content = vec![("shown", shown.clone())];
        if let Some(raw) = raw {
            content.push(("raw", raw));
        }
        self.record(
            "tool.result",
            None,
            json!({
                "tool": tool_name,
                "toolCallId": tool_call_id,
                "isError": is_error,
                "exitCode": exit_code,
                "status": details["status"],
                "durationMs": details["durationMs"],
                "shownBytes": shown.len(),
                "rawBytes": raw_bytes,
                "truncated": truncation["truncated"].as_bool().unwrap_or(raw_bytes > shown.len() as u64),
                "truncatedBy": truncation["truncatedBy"],
                "fullOutputPath": details["fullOutputPath"],
            }),
            content,
        );
        let check = {
            let mut decisions = self.decisions();
            let check = decisions.running_checks.remove(tool_call_id);
            if let (Some((kind, _)), Some(run)) = (check.as_ref(), decisions.run.as_mut()) {
                run.checks.push((kind, !failed));
            }
            check
        };
        if let Some((kind, command)) = check {
            self.record(
                "verification",
                None,
                json!({
                    "kind": kind,
                    "command": command,
                    "tool": tool_name,
                    "toolCallId": tool_call_id,
                    "passed": !failed,
                    "exitCode": exit_code,
                    "durationMs": details["durationMs"],
                }),
                Vec::new(),
            );
        }
        if failed {
            self.error_detected(
                "tool_error",
                json!({ "tool": tool_name, "toolCallId": tool_call_id, "exitCode": exit_code }),
            );
        }
    }
}

/// Subscribe the recorder to `agent`'s events. The listener only queues
/// records, so it never slows the run.
pub async fn observe_agent_events(agent: &pa_agent::agent::Agent, recorder: Arc<Recorder>) {
    agent
        .subscribe(move |event, _signal| {
            recorder.observe(&event);
            Box::pin(async { Ok(()) })
        })
        .await;
}

/// Wrap `stream_fn` so every model call writes `context.built` and
/// `api.call`; the wrapper chains the existing request hooks.
pub fn instrument_stream_fn(recorder: Arc<Recorder>, stream_fn: StreamFn) -> StreamFn {
    Arc::new(move |model, context, options| {
        let recorder = Arc::clone(&recorder);
        let stream_fn = Arc::clone(&stream_fn);
        Box::pin(async move {
            let step = recorder.next_step();
            recorder.context_built(step, &context);
            let call = Arc::new(Call {
                step,
                provider: model.provider.clone(),
                model: model.id.clone(),
                settings: json!({
                    "reasoning": options.reasoning,
                    "maxTokens": options.max_tokens,
                    "temperature": options.temperature,
                    "serviceTier": options.service_tier,
                }),
                messages: context.messages.len(),
                tools: context.tools.len(),
                started: Instant::now(),
                state: std::sync::Mutex::default(),
                recorder,
            });
            let options = chain_hooks(&call, options);
            match stream_fn(model, context, options).await {
                Err(error) => {
                    call.finish(None, Some(format!("{error:#}")));
                    Err(error)
                }
                Ok(stream) => Ok(Box::new(CaptureStream {
                    inner: stream,
                    call,
                }) as Box<dyn ModelStream>),
            }
        })
    })
}

fn chain_hooks(call: &Arc<Call>, mut options: StreamRequestOptions) -> StreamRequestOptions {
    let inner_payload: Option<OnPayloadHook> = options.on_payload.take();
    let payload_call = Arc::clone(call);
    options.on_payload = Some(Arc::new(move |payload, model| {
        let next = inner_payload
            .as_ref()
            .and_then(|hook| hook(payload.clone(), model))
            .unwrap_or(payload);
        payload_call.state().request = serde_json::to_string(&next).ok();
        Some(next)
    }));
    let inner_response: Option<OnResponseHook> = options.on_response.take();
    let response_call = Arc::clone(call);
    options.on_response = Some(Arc::new(move |response, model| {
        {
            let mut state = response_call.state();
            state.status = Some(response.status);
            state.first_byte_ms = Some(response_call.elapsed_ms());
        }
        if let Some(hook) = inner_response.as_ref() {
            hook(response, model);
        }
    }));
    let inner_detail: Option<OnProviderDetailHook> = options.on_provider_detail.take();
    let detail_call = Arc::clone(call);
    options.on_provider_detail = Some(Arc::new(move |detail: Value| {
        detail_call.state().detail = Some(detail.clone());
        if let Some(hook) = inner_detail.as_ref() {
            hook(detail);
        }
    }));
    options
}

/// One model call in flight.
struct Call {
    recorder: Arc<Recorder>,
    step: u64,
    provider: String,
    model: String,
    settings: Value,
    messages: usize,
    tools: usize,
    started: Instant,
    state: std::sync::Mutex<CallState>,
}

#[derive(Default)]
struct CallState {
    request: Option<String>,
    status: Option<u16>,
    first_byte_ms: Option<f64>,
    first_token_ms: Option<f64>,
    detail: Option<Value>,
    finished: bool,
}

impl Call {
    fn state(&self) -> std::sync::MutexGuard<'_, CallState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn elapsed_ms(&self) -> f64 {
        (self.started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0
    }

    fn mark_first_token(&self) {
        let elapsed = self.elapsed_ms();
        self.state().first_token_ms.get_or_insert(elapsed);
    }

    /// Write `api.call` once: from the terminal event, a failed start, or
    /// a stream dropped early (`message` absent: aborted).
    fn finish(&self, message: Option<&AssistantMessage>, failure: Option<String>) {
        let total_ms = self.elapsed_ms();
        let (request, status, first_byte_ms, first_token_ms, detail) = {
            let mut state = self.state();
            if state.finished {
                return;
            }
            state.finished = true;
            (
                state.request.take(),
                state.status,
                state.first_byte_ms,
                state.first_token_ms,
                state.detail.take().unwrap_or(Value::Null),
            )
        };
        let reasoning: String = message
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Thinking(thinking) => Some(thinking.thinking.as_str()),
                        AssistantContent::Text(_) | AssistantContent::ToolCall(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let stop_reason = match message.map(|message| message.stop_reason) {
            Some(StopReason::Stop) => "stop",
            Some(StopReason::Length) => "length",
            Some(StopReason::ToolUse) => "toolUse",
            Some(StopReason::Aborted) | None if failure.is_none() => "aborted",
            Some(StopReason::Error | StopReason::Aborted) | None => "error",
        };
        let error = message
            .and_then(|message| message.error_message.clone())
            .or(failure);
        let usage = message.map_or_else(pa_agent::types::Usage::zero, |message| {
            message.usage.clone()
        });
        let repaired_chunks = detail["repairedChunks"].as_u64().unwrap_or(0);
        let repaired_tool_arguments = detail["repairedToolArguments"].clone();
        let tool_calls = message.map_or(0, |message| {
            message
                .content
                .iter()
                .filter(|block| matches!(block, AssistantContent::ToolCall(_)))
                .count()
        });
        let data = json!({
            "provider": self.provider,
            "modelRequested": self.model,
            "modelReturned": message.map(|message| {
                message.response_model.clone().unwrap_or_else(|| message.model.clone())
            }),
            "responseId": message.and_then(|message| message.response_id.clone()),
            "settings": self.settings,
            "requestBytes": request.as_ref().map(String::len),
            "requestMessages": self.messages,
            "requestTools": self.tools,
            "usage": {
                "cacheHit": usage.cache_read,
                "cacheMiss": usage.input,
                "cacheWrite": usage.cache_write,
                "output": usage.output,
                "reasoning": detail["usage"]["completion_tokens_details"]["reasoning_tokens"],
                "total": usage.total_tokens,
                "cost": usage.cost.total,
                "providerUsage": detail["usage"],
            },
            "latencyMs": {
                "firstByte": first_byte_ms,
                "firstToken": first_token_ms,
                "total": total_ms,
            },
            "http": { "status": status },
            "stopReason": stop_reason,
            "error": error,
            "parse": {
                "clean": repaired_chunks == 0
                    && repaired_tool_arguments.as_array().is_none_or(Vec::is_empty),
                "repairedChunks": repaired_chunks,
                "repairedToolArguments": repaired_tool_arguments,
            },
            "toolCalls": tool_calls,
            "reasoningChars": reasoning.chars().count(),
            "offPeak": self.recorder.off_peak_now(),
        });
        if let Some(run) = self.recorder.decisions().run.as_mut() {
            run.cost += usage.cost.total;
        }
        let mut content = Vec::new();
        if let Some(request) = request {
            content.push(("request", request));
        }
        if !reasoning.is_empty() {
            content.push(("reasoning", reasoning));
        }
        self.recorder
            .record("api.call", Some(self.step), data, content);
        if stop_reason == "error" {
            self.recorder.error_detected(
                "provider_error",
                json!({ "httpStatus": status, "message": error }),
            );
        }
    }
}

/// The provider stream, observed for first-token time and the terminal
/// event.
struct CaptureStream {
    inner: Box<dyn ModelStream>,
    call: Arc<Call>,
}

impl ModelStream for CaptureStream {
    fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
        let call = Arc::clone(&self.call);
        Box::pin(async move {
            let event = self.inner.next_event().await;
            match &event {
                None => call.finish(None, None),
                Some(
                    AssistantMessageEvent::TextStart { .. }
                    | AssistantMessageEvent::TextDelta { .. }
                    | AssistantMessageEvent::ThinkingStart { .. }
                    | AssistantMessageEvent::ThinkingDelta { .. }
                    | AssistantMessageEvent::ToolCallStart { .. }
                    | AssistantMessageEvent::ToolCallDelta { .. },
                ) => call.mark_first_token(),
                Some(AssistantMessageEvent::Done { message, .. }) => {
                    call.finish(Some(message), None);
                }
                Some(AssistantMessageEvent::Error { error, .. }) => call.finish(Some(error), None),
                Some(
                    AssistantMessageEvent::Start { .. }
                    | AssistantMessageEvent::TextEnd { .. }
                    | AssistantMessageEvent::ThinkingEnd { .. }
                    | AssistantMessageEvent::ToolCallEnd { .. },
                ) => {}
            }
            event
        })
    }

    fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
        self.inner.result()
    }

    fn close(&mut self) {
        self.call.finish(None, None);
        self.inner.close();
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        self.call.finish(None, None);
    }
}

fn user_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                UserPart::Text(text) => Some(text.text.as_str()),
                UserPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn user_images(content: &UserContent) -> usize {
    match content {
        UserContent::Text(_) => 0,
        UserContent::Parts(parts) => parts
            .iter()
            .filter(|part| matches!(part, UserPart::Image(_)))
            .count(),
    }
}

/// The shell command or Python code a tool call runs.
fn command_of(args: &Value) -> Option<String> {
    args["command"]
        .as_str()
        .or_else(|| args["code"].as_str())
        .map(str::to_string)
}

/// Whether `command` runs tests, a linter or a type checker.
fn verification_kind(command: &str) -> Option<&'static str> {
    static KINDS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    let kinds = KINDS.get_or_init(|| {
        [
            ("test", r"\b(?:cargo (?:test|nextest)|pytest|py\.test|go test|(?:npm|yarn|pnpm|bun) (?:run )?test|jest|vitest|mocha|rspec|phpunit|ctest|mvn test|gradle test|make test|deno test|tox)\b"),
            ("lint", r"\b(?:cargo clippy|clippy|eslint|ruff(?: check)?|flake8|pylint|golangci-lint|rubocop|shellcheck|stylelint|biome (?:lint|check)|cargo fmt --check|cargo fmt --all --check)\b"),
            ("typecheck", r"\b(?:tsc|mypy|pyright|cargo check)\b"),
        ]
        .into_iter()
        .map(|(kind, pattern)| (kind, Regex::new(pattern).expect("valid verification pattern")))
        .collect()
    });
    kinds
        .iter()
        .find(|(_, pattern)| pattern.is_match(command).unwrap_or(false))
        .map(|(kind, _)| *kind)
}

/// The exit code the bash tool appends to a failed command's output.
fn exit_code(shown: &str) -> Option<i64> {
    let marker = "Command exited with code ";
    let start = shown.rfind(marker)? + marker.len();
    let digits: String = shown[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '-')
        .collect();
    digits.parse().ok()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn sha256(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_checks_and_exit_codes() {
        assert_eq!(
            verification_kind("cd app && cargo test -p core"),
            Some("test")
        );
        assert_eq!(
            verification_kind("npm run test -- --watch=false"),
            Some("test")
        );
        assert_eq!(verification_kind("ruff check ."), Some("lint"));
        assert_eq!(verification_kind("npx tsc --noEmit"), Some("typecheck"));
        assert_eq!(verification_kind("ls -la"), None);
        assert_eq!(exit_code("boom\n\nCommand exited with code 2"), Some(2));
        assert_eq!(exit_code("all good"), None);
    }
}
