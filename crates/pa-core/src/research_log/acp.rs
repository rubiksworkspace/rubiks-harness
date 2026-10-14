//! The IDE side of a session as Prime Agent sees it over ACP. Every frame in
//! both directions goes to `<session>.acp.jsonl` (method, id and size always;
//! the frame itself with content consent), and from the frames the session
//! log gets:
//!
//! - `acp.session_bound`: the IDE's ACP session id for this session.
//! - `acp.prompt` / `acp.turn_end`: what each prompt attached (text,
//!   resources, images) and how its turn ended.
//! - `steering`: a cancel or a message sent while a turn runs, with the time
//!   since the IDE last heard from the agent.
//! - `permission`: a `session/request_permission` the agent sent, the
//!   user's answer and how long it took.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};

use super::store::{acp_stream_file_name, Store};
use super::{now_iso, Consent, ResearchConfig, HARNESS_COMMIT};

/// Watches one ACP connection's frames for one session.
pub struct AcpTap {
    store: Arc<Store>,
    session_id: String,
    keep_content: bool,
    state: Mutex<TapState>,
}

#[derive(Default)]
struct TapState {
    acp_session_id: Option<String>,
    /// Requests Prime Agent sent the IDE, by JSON-RPC id: method, when, params.
    sent_requests: HashMap<String, (String, Instant, Value)>,
    /// The running `session/prompt` request's id and start.
    prompt: Option<(String, Instant)>,
    /// The newest `session/update` the IDE was sent: when, and its kind.
    last_update: Option<(Instant, String)>,
    /// The IDE's `session/new` request ids; their responses carry the ACP
    /// session id.
    session_new_requests: Vec<String>,
}

impl AcpTap {
    /// A tap for `session_id`, or `None` when the research log is off.
    #[must_use]
    pub fn open(agent_dir: &Path, session_id: &str) -> Option<Arc<AcpTap>> {
        let config = ResearchConfig::resolve(
            &crate::settings::SettingsManager::create(agent_dir, agent_dir).get_research(),
        );
        Self::with_config(agent_dir, session_id, &config)
    }

    pub(super) fn with_config(
        agent_dir: &Path,
        session_id: &str,
        config: &ResearchConfig,
    ) -> Option<Arc<AcpTap>> {
        config.enabled.then(|| {
            Arc::new(AcpTap {
                store: Store::for_agent_dir(agent_dir),
                session_id: session_id.to_string(),
                keep_content: config.consent == Consent::Content,
                state: Mutex::default(),
            })
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, TapState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The IDE's ACP session id for this session (`acp.session_bound`).
    fn set_acp_session(&self, acp_session_id: &str) {
        self.state().acp_session_id = Some(acp_session_id.to_string());
        self.record("acp.session_bound", json!({}));
    }

    /// A line the IDE sent.
    pub fn incoming(&self, line: &str) {
        let frame: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
        self.stream("in", &frame, line.trim().len());
        let method = frame["method"].as_str().unwrap_or_default();
        let id = id_of(&frame);
        match (method, id) {
            ("session/new", Some(id)) => self.state().session_new_requests.push(id),
            ("session/prompt", Some(id)) => {
                let (running, since_update) = {
                    let mut state = self.state();
                    let running = state.prompt.is_some();
                    let since_update = since_last_update(&state);
                    state.prompt = Some((id, Instant::now()));
                    (running, since_update)
                };
                self.record("acp.prompt", prompt_summary(&frame["params"]["prompt"]));
                if running {
                    self.record(
                        "steering",
                        json!({ "kind": "mid_run_message", "sinceLastUpdate": since_update }),
                    );
                }
            }
            ("session/cancel", _) => {
                let (running_ms, since_update) = {
                    let state = self.state();
                    (
                        state
                            .prompt
                            .as_ref()
                            .map(|(_, started)| elapsed_ms(*started)),
                        since_last_update(&state),
                    )
                };
                self.record(
                    "steering",
                    json!({
                        "kind": "cancel",
                        "turnRunningMs": running_ms,
                        "sinceLastUpdate": since_update,
                    }),
                );
            }
            ("", Some(id)) => {
                // A response to a request Prime Agent sent the IDE.
                let sent = self.state().sent_requests.remove(&id);
                if let Some((method, sent_at, params)) = sent {
                    if method == "session/request_permission" {
                        self.record("permission", permission_record(&params, &frame, sent_at));
                    }
                }
            }
            _ => {}
        }
    }

    /// A frame Prime Agent sent the IDE.
    pub fn outgoing(&self, frame: &Value) {
        let bytes = serde_json::to_string(frame).map_or(0, |text| text.len());
        self.stream("out", frame, bytes);
        let method = frame["method"].as_str();
        let id = id_of(frame);
        match (method, id) {
            (Some("session/update"), _) => {
                let kind = frame["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("update")
                    .to_string();
                self.state().last_update = Some((Instant::now(), kind));
            }
            (Some(method), Some(id)) => {
                self.state().sent_requests.insert(
                    id,
                    (method.to_string(), Instant::now(), frame["params"].clone()),
                );
            }
            (None, Some(id)) => {
                let session_new = {
                    let mut state = self.state();
                    let before = state.session_new_requests.len();
                    state.session_new_requests.retain(|request| *request != id);
                    state.session_new_requests.len() != before
                };
                if session_new {
                    if let Some(acp_session_id) = frame["result"]["sessionId"].as_str() {
                        self.set_acp_session(acp_session_id);
                    }
                }
                let finished = {
                    let mut state = self.state();
                    match &state.prompt {
                        Some((prompt_id, started)) if *prompt_id == id => {
                            let started = *started;
                            state.prompt = None;
                            Some(started)
                        }
                        _ => None,
                    }
                };
                if let Some(started) = finished {
                    self.record(
                        "acp.turn_end",
                        json!({
                            "stopReason": frame["result"]["stopReason"],
                            "error": frame["error"]["message"],
                            "durationMs": elapsed_ms(started),
                        }),
                    );
                }
            }
            (Some(_) | None, None) => {}
        }
    }

    /// One frame of the stream file.
    fn stream(&self, direction: &str, frame: &Value, bytes: usize) {
        let acp_session_id = self.state().acp_session_id.clone();
        let mut record = json!({
            "ts": now_iso(),
            "event_type": "acp.message",
            "session_id": self.session_id,
            "acp_session_id": acp_session_id,
            "data": {
                "direction": direction,
                "method": frame["method"],
                "id": frame["id"],
                "update": frame["params"]["update"]["sessionUpdate"],
                "bytes": bytes,
            },
        });
        if self.keep_content {
            record["data"]["frame"] = frame.clone();
        }
        self.store
            .append_line(acp_stream_file_name(&self.session_id), record);
    }

    fn record(&self, event_type: &str, data: Value) {
        let acp_session_id = self.state().acp_session_id.clone();
        let mut record = json!({
            "ts": now_iso(),
            "event_type": event_type,
            "run_id": Value::Null,
            "session_id": self.session_id,
            "acp_session_id": acp_session_id,
            "step": Value::Null,
            "harness_version": HARNESS_COMMIT,
        });
        record["data"] = data;
        self.store
            .append(&self.session_id, record, Vec::new(), self.keep_content);
    }
}

fn id_of(frame: &Value) -> Option<String> {
    match &frame["id"] {
        Value::String(id) => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn since_last_update(state: &TapState) -> Value {
    match &state.last_update {
        Some((at, kind)) => json!({ "ms": elapsed_ms(*at), "kind": kind }),
        None => Value::Null,
    }
}

/// What a prompt attached: block counts by type, text size, and the
/// resources (files the IDE attached or linked).
fn prompt_summary(blocks: &Value) -> Value {
    let mut by_type: HashMap<String, usize> = HashMap::new();
    let mut chars = 0usize;
    let mut resources = Vec::new();
    for block in blocks.as_array().into_iter().flatten() {
        let kind = block["type"].as_str().unwrap_or("unknown");
        *by_type.entry(kind.to_string()).or_default() += 1;
        match kind {
            "text" => {
                chars += block["text"]
                    .as_str()
                    .map_or(0, |text| text.chars().count());
            }
            "resource_link" => {
                resources.push(json!({ "uri": block["uri"], "name": block["name"] }));
            }
            "resource" => resources.push(json!({
                "uri": block["resource"]["uri"],
                "bytes": block["resource"]["text"].as_str().map(str::len),
            })),
            _ => {}
        }
    }
    json!({ "blocks": by_type, "chars": chars, "resources": resources })
}

fn permission_record(params: &Value, response: &Value, sent_at: Instant) -> Value {
    let outcome = &response["result"]["outcome"];
    let chosen = outcome["optionId"].as_str();
    let options = params["options"].as_array().cloned().unwrap_or_default();
    let chosen_kind = chosen.and_then(|chosen| {
        options
            .iter()
            .find(|option| option["optionId"].as_str() == Some(chosen))
            .and_then(|option| option["kind"].as_str().map(str::to_string))
    });
    json!({
        "toolCall": {
            "id": params["toolCall"]["toolCallId"],
            "title": params["toolCall"]["title"],
            "kind": params["toolCall"]["kind"],
        },
        "options": options
            .iter()
            .map(|option| json!({ "id": option["optionId"], "kind": option["kind"], "name": option["name"] }))
            .collect::<Vec<_>>(),
        "outcome": outcome["outcome"],
        "decision": chosen,
        "decisionKind": chosen_kind,
        "error": response["error"]["message"],
        "decisionMs": elapsed_ms(sent_at),
    })
}
