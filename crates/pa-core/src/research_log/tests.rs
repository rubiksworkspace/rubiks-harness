use std::path::Path;
use std::sync::Arc;

use pa_agent::stream::{
    event_stream, AssistantMessageEvent, LlmContext, ModelStream, StreamFn, StreamRequestOptions,
};
use pa_agent::types::{
    AgentEvent, AgentMessage, AgentToolResult, AssistantContent, Message, StopReason, TextContent,
    ThinkingContent, ToolResultContent, Usage, UsageCost, UserContent, UserMessage,
};
use serde_json::{json, Value};

use super::store::Store;
use super::*;
use crate::session_engine::auto_retry::{AutoRetryEvent, RetryStartReason};

fn model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "deepseek-v4-flash".to_string(),
        name: "Flash".to_string(),
        api: "openai-completions".to_string(),
        provider: "deepseek".to_string(),
        base_url: "https://api.deepseek.test".to_string(),
        reasoning: true,
        cost: UsageCost::default(),
        context_window: 1_000_000,
        max_tokens: 64_000,
        max_tokens_explicit: false,
    }
}

fn reply(model: &pa_agent::types::Model) -> pa_agent::types::AssistantMessage {
    pa_agent::types::AssistantMessage {
        content: vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "Run the tests.".to_string(),
                thinking_signature: None,
                redacted: None,
            }),
            AssistantContent::Text(TextContent {
                text: "Running them.".to_string(),
                text_signature: None,
            }),
        ],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: Some("deepseek-flash".to_string()),
        response_id: Some("resp-1".to_string()),
        diagnostics: None,
        usage: Usage {
            input: 189,
            output: 159,
            cache_read: 5888,
            cache_write: 0,
            total_tokens: 6236,
            cost: UsageCost {
                total: 0.0001,
                ..UsageCost::default()
            },
        },
        stop_reason: StopReason::ToolUse,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
    }
}

/// A provider that calls the request hooks like the `DeepSeek` client, then
/// streams one thinking block and finishes.
fn provider() -> StreamFn {
    Arc::new(|model, _context, options: StreamRequestOptions| {
        Box::pin(async move {
            let (handle, consumer) = event_stream();
            if let Some(hook) = options.on_payload.as_ref() {
                hook(
                    json!({"messages": [{"role": "user", "content": "key sk-abcdefghijklmnopqrstuvwx"}]}),
                    &model,
                );
            }
            if let Some(hook) = options.on_response.as_ref() {
                hook(
                    pa_agent::stream::ProviderResponse {
                        status: 200,
                        headers: std::collections::BTreeMap::default(),
                    },
                    &model,
                );
            }
            let message = reply(&model);
            handle.push(AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: message.clone(),
            });
            if let Some(hook) = options.on_provider_detail.as_ref() {
                hook(json!({
                    "usage": {"completion_tokens_details": {"reasoning_tokens": 112}},
                    "repairedChunks": 0,
                    "repairedToolArguments": ["bash"],
                }));
            }
            handle.push(AssistantMessageEvent::Done {
                reason: StopReason::ToolUse,
                message,
            });
            Ok(Box::new(consumer) as Box<dyn ModelStream>)
        })
    })
}

fn info(cwd: &Path) -> RunInfo {
    RunInfo {
        cwd: cwd.to_path_buf(),
        provider: "deepseek".to_string(),
        model_id: "deepseek-v4-flash".to_string(),
        context_window: 1_000_000,
        max_tokens: 64_000,
        thinking_level: "high".to_string(),
        service_tier: None,
        prices: json!({"input": 0.15, "output": 0.6}),
        system_prompt: "You are Prime Agent.".to_string(),
        tools: vec![json!({"name": "bash", "description": "Run a command", "parameters": {}})],
        skills: vec!["refine".to_string()],
        harness_settings: json!({"compaction": {"enabled": true}}),
        model_prompt_extras: None,
        rlm_depth: 0,
        daemon_session_id: Some("d-1".to_string()),
    }
}

fn config(consent: Consent) -> ResearchConfig {
    ResearchConfig {
        enabled: true,
        consent,
        participant_id: Some("p-017".to_string()),
        task: Some("fix the parser".to_string()),
        off_peak_utc: None,
    }
}

fn context(text: &str) -> LlmContext {
    LlmContext {
        system_prompt: Some("You are Prime Agent.".to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })],
        tools: Vec::new(),
    }
}

async fn call(stream_fn: &StreamFn, text: &str) {
    let mut stream = stream_fn(model(), context(text), StreamRequestOptions::default())
        .await
        .expect("stream");
    while stream.next_event().await.is_some() {}
}

fn bash(recorder: &Recorder, id: &str, command: &str, output: &str, is_error: bool) {
    recorder.observe(&AgentEvent::ToolExecutionStart {
        tool_call_id: id.to_string(),
        tool_name: "bash".to_string(),
        args: json!({ "command": command }),
    });
    recorder.observe(&AgentEvent::ToolExecutionEnd {
        tool_call_id: id.to_string(),
        tool_name: "bash".to_string(),
        result: AgentToolResult {
            content: vec![ToolResultContent::text(output)],
            details: json!({}),
            terminate: None,
        },
        is_error,
    });
}

/// The records once `run.start` (written from its own thread) has landed.
fn records(recorder: &Recorder, agent_dir: &Path) -> Vec<Value> {
    for _ in 0..200 {
        assert!(recorder.flush(), "the writer drains");
        let text = std::fs::read_to_string(session_log_path(agent_dir, "s-1")).unwrap_or_default();
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("one JSON record per line"))
            .collect();
        if records
            .iter()
            .any(|record| record["event_type"] == "run.start")
        {
            return records;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("run.start never landed");
}

#[tokio::test]
async fn a_run_records_calls_decisions_and_their_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Recorder::start(
        dir.path(),
        "s-1",
        config(Consent::Content),
        info(dir.path()),
    )
    .unwrap();
    let stream_fn = instrument_stream_fn(Arc::clone(&recorder), provider());

    recorder.observe(&AgentEvent::MessageStart {
        message: AgentMessage::Standard(Message::User(UserMessage {
            content: UserContent::Text("Fix the parser".to_string()),
            timestamp: 0,
        })),
    });
    call(&stream_fn, "Fix the parser").await;
    bash(
        &recorder,
        "c1",
        "cargo test -p parser",
        "1 failed\n\nCommand exited with code 101",
        true,
    );
    call(&stream_fn, "again").await;
    for id in ["c2", "c3", "c4"] {
        bash(&recorder, id, "ls", "README.md", false);
    }
    recorder.note_auto_retry(&AutoRetryEvent::Start {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 1000,
        error_message: "429".to_string(),
        reason: RetryStartReason::Quick,
    });
    recorder.note_compaction(90_000, 12_000, 40, "entry-9", "Summary of the work");
    recorder.observe(&AgentEvent::MessageStart {
        message: AgentMessage::Standard(Message::User(UserMessage {
            content: UserContent::Text("Stop looping".to_string()),
            timestamp: 0,
        })),
    });

    let records = records(&recorder, dir.path());
    let events: Vec<(&str, u64)> = records
        .iter()
        .filter(|record| record["event_type"] != "run.start")
        .map(|record| {
            (
                record["event_type"].as_str().unwrap(),
                record["step"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        events,
        [
            ("user.prompt", 0),
            ("context.built", 1),
            ("api.call", 1),
            ("tool.result", 1),
            ("verification", 1),
            ("error.detected", 1),
            ("context.built", 2),
            ("correction.delivered", 2),
            ("api.call", 2),
            ("tool.result", 2),
            ("tool.result", 2),
            // The third identical call is a loop as soon as it starts.
            ("error.detected", 2),
            ("tool.result", 2),
            ("api.retry", 2),
            ("context.compacted", 2),
            ("user.prompt", 2),
            ("correction.delivered", 2),
        ]
    );
    for record in &records {
        assert_eq!(record["run_id"], recorder.run_id());
        assert_eq!(record["session_id"], "s-1");
        assert_eq!(record["harness_version"], recorder.harness_version());
    }

    let by_type = |event_type: &str| -> Vec<&Value> {
        records
            .iter()
            .filter(|record| record["event_type"] == event_type)
            .map(|record| &record["data"])
            .collect()
    };
    let api = by_type("api.call")[0];
    assert_eq!(api["modelRequested"], "deepseek-v4-flash");
    assert_eq!(api["modelReturned"], "deepseek-flash");
    assert_eq!(
        api["usage"],
        json!({
            "cacheHit": 5888, "cacheMiss": 189, "cacheWrite": 0, "output": 159,
            "reasoning": 112, "total": 6236, "cost": 0.0001,
            "providerUsage": {"completion_tokens_details": {"reasoning_tokens": 112}},
        })
    );
    assert_eq!(api["http"]["status"], 200);
    assert_eq!(api["parse"]["clean"], false);
    assert_eq!(api["stopReason"], "toolUse");
    assert!(api["latencyMs"]["firstToken"].is_number());

    // The payload blob is stored, with the key stripped.
    let request_hash = api["request"]["sha256"].as_str().unwrap();
    let request = std::fs::read_to_string(blob_path(dir.path(), request_hash)).unwrap();
    assert!(request.contains("<redacted>") && !request.contains("sk-abcdef"));

    assert_eq!(
        by_type("verification")[0],
        &json!({"kind": "test", "command": "cargo test -p parser", "tool": "bash",
                "toolCallId": "c1", "passed": false, "exitCode": 101, "durationMs": null})
    );
    let errors = by_type("error.detected");
    assert_eq!(
        (errors[0]["type"].as_str(), errors[1]["type"].as_str()),
        (Some("tool_error"), Some("loop"))
    );
    let corrections = by_type("correction.delivered");
    assert_eq!(
        corrections
            .iter()
            .map(|correction| (
                correction["answers"].as_str().unwrap(),
                correction["form"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [("e1", "error_message"), ("e2", "user_message")]
    );

    let start = &records
        .iter()
        .find(|record| record["event_type"] == "run.start")
        .unwrap()["data"];
    assert_eq!(
        start["participant"],
        json!({"id": "p-017", "consent": "content"})
    );
    assert_eq!(start["work"]["task"], "fix the parser");
    assert_eq!(start["model"]["requested"], "deepseek-v4-flash");
    assert!(start["upstream"]["changes"].is_array());
}

#[tokio::test]
async fn metadata_consent_keeps_hashes_but_no_content() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Recorder::start(
        dir.path(),
        "s-1",
        config(Consent::Metadata),
        info(dir.path()),
    )
    .unwrap();
    let stream_fn = instrument_stream_fn(Arc::clone(&recorder), provider());
    call(&stream_fn, "hi").await;
    let records = records(&recorder, dir.path());
    let api = records
        .iter()
        .find(|record| record["event_type"] == "api.call")
        .unwrap();
    assert_eq!(api["data"]["request"]["stored"], false);
    assert!(!research_dir(dir.path()).join("blobs").exists());
}

#[test]
fn settings_and_environment_resolve_the_config() {
    let settings = ResearchSettings {
        consent: Some("content".to_string()),
        off_peak_utc: Some("16:30-00:30".to_string()),
        ..ResearchSettings::default()
    };
    let none = |_: &str| None;
    let config = ResearchConfig::resolve_with(&settings, none);
    assert_eq!(
        config,
        ResearchConfig {
            enabled: true,
            consent: Consent::Content,
            participant_id: None,
            task: None,
            off_peak_utc: Some((990, 30)),
        }
    );
    let off = ResearchConfig::resolve_with(&settings, |name| {
        (name == "PRIME_AGENT_RESEARCH").then(|| "0".to_string())
    });
    assert!(!off.enabled);
    assert!(in_window((990, 30), 1000) && in_window((990, 30), 10) && !in_window((990, 30), 600));
}

/// The engine builds a recorder from the `research` settings and routes the
/// session's model calls and agent events through it.
#[tokio::test]
async fn engine_sessions_write_a_research_log_when_settings_turn_it_on() {
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    use crate::session_engine::provider_adapter::{json_round_trip, real_stream_fn};

    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"research": {"enabled": true, "participantId": "p-1"}}"#,
    )
    .unwrap();
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            api: Some("research-log-faux".to_string()),
            provider: Some("faux-research".to_string()),
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "faux-1".to_string(),
                name: Some("Faux".to_string()),
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(100_000),
                max_tokens: Some(4_096),
            }]),
            ..Default::default()
        });
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "ok",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    let model = registration.get_model();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: agent_dir.clone(),
        model: Some(json_round_trip(&model).unwrap()),
        stream_fn: Some(real_stream_fn(None, model.clone())),
        tools: Vec::new(),
        ..Default::default()
    })
    .await
    .unwrap();
    engine
        .session
        .prompt("hello", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    registration.unregister();

    let recorder = engine
        .session
        .research_log()
        .expect("the session has a recorder");
    let session_id = engine.session.session_id().await;
    let mut events = Vec::new();
    for _ in 0..200 {
        assert!(recorder.flush());
        let text =
            std::fs::read_to_string(session_log_path(&agent_dir, &session_id)).unwrap_or_default();
        events = text
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["event_type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        if events.iter().any(|event| event == "run.start")
            && events.iter().any(|event| event == "run.end")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    events.retain(|event| event != "run.start");
    assert_eq!(
        events,
        ["user.prompt", "context.built", "api.call", "run.end"]
    );
}

fn session_records(agent_dir: &Path, session_id: &str) -> Vec<Value> {
    std::fs::read_to_string(session_log_path(agent_dir, session_id))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON record per line"))
        .collect()
}

#[test]
fn the_acp_tap_records_frames_prompts_steering_and_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let tap = AcpTap::with_config(dir.path(), "s-1", &config(Consent::Content)).unwrap();
    tap.incoming(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/w"}}"#);
    tap.outgoing(&json!({"jsonrpc":"2.0","id":1,"result":{"sessionId":"acp-9"}}));
    tap.incoming(r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"acp-9","prompt":[{"type":"text","text":"Fix it"},{"type":"resource_link","uri":"file:///w/a.rs","name":"a.rs"}]}}"#);
    tap.outgoing(&json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acp-9","update":{"sessionUpdate":"tool_call"}}}));
    tap.outgoing(&json!({"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{
        "toolCall":{"toolCallId":"t1","title":"Run rm","kind":"execute"},
        "options":[{"optionId":"allow","kind":"allow_once","name":"Allow"},{"optionId":"no","kind":"reject_once","name":"Reject"}]}}));
    tap.incoming(r#"{"jsonrpc":"2.0","id":"p1","result":{"outcome":{"outcome":"selected","optionId":"no"}}}"#);
    tap.incoming(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"acp-9"}}"#);
    tap.outgoing(&json!({"jsonrpc":"2.0","id":2,"result":{"stopReason":"cancelled"}}));
    assert!(Store::for_agent_dir(dir.path()).flush(std::time::Duration::from_secs(5)));

    let records = session_records(dir.path(), "s-1");
    let events: Vec<&str> = records
        .iter()
        .map(|record| record["event_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        events,
        [
            "acp.session_bound",
            "acp.prompt",
            "permission",
            "steering",
            "acp.turn_end"
        ]
    );
    assert!(records
        .iter()
        .all(|record| record["acp_session_id"] == "acp-9"));
    assert_eq!(
        records[1]["data"],
        json!({"blocks": {"text": 1, "resource_link": 1}, "chars": 6,
               "resources": [{"uri": "file:///w/a.rs", "name": "a.rs"}]})
    );
    let permission = &records[2]["data"];
    assert_eq!(
        (
            &permission["toolCall"]["title"],
            &permission["decision"],
            &permission["decisionKind"]
        ),
        (&json!("Run rm"), &json!("no"), &json!("reject_once"))
    );
    assert_eq!(records[3]["data"]["kind"], "cancel");
    assert_eq!(records[3]["data"]["sinceLastUpdate"]["kind"], "tool_call");
    assert_eq!(records[4]["data"]["stopReason"], "cancelled");

    let stream: Vec<Value> = std::fs::read_to_string(acp_stream_path(dir.path(), "s-1"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(stream.len(), 8);
    assert_eq!(stream[0]["data"]["direction"], "in");
    assert_eq!(stream[0]["data"]["method"], "session/new");
    assert_eq!(stream[3]["data"]["update"], "tool_call");
    assert!(
        stream[2]["data"]["frame"].is_object(),
        "content consent keeps frames"
    );
    assert_eq!(
        session_logs(dir.path()),
        [session_log_path(dir.path(), "s-1")]
    );
}

#[tokio::test]
async fn a_run_ends_with_its_diff_cost_and_checks() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(status.status.success(), "git {args:?}: {status:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "start"]);

    let recorder =
        Recorder::start(dir.path(), "s-1", config(Consent::Content), info(&repo)).unwrap();
    let _ = records(&recorder, dir.path());
    let stream_fn = instrument_stream_fn(Arc::clone(&recorder), provider());
    recorder.observe(&AgentEvent::AgentStart);
    call(&stream_fn, "change a.txt").await;
    bash(&recorder, "c1", "cargo test", "ok", false);
    std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
    recorder.observe(&AgentEvent::AgentEnd {
        messages: vec![AgentMessage::Standard(Message::Assistant(reply(&model())))],
    });

    let mut end = Value::Null;
    for _ in 0..200 {
        assert!(recorder.flush());
        if let Some(record) = session_records(dir.path(), "s-1")
            .into_iter()
            .find(|record| record["event_type"] == "run.end")
        {
            end = record;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let data = &end["data"];
    assert_eq!(data["modelCalls"], 1);
    assert_eq!(data["cost"], 0.0001);
    assert_eq!(data["checks"], json!([{"kind": "test", "passed": true}]));
    assert_eq!(data["stopReason"], "tooluse");
    assert_eq!(data["repo"]["committed"], false);
    assert_eq!(data["repo"]["uncommittedFiles"], 1);
    let diff = std::fs::read_to_string(blob_path(
        dir.path(),
        data["diff"]["sha256"].as_str().unwrap(),
    ))
    .unwrap();
    assert!(diff.contains("+two"));
}
