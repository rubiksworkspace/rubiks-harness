#![cfg(unix)]
#![allow(clippy::too_many_lines)]

//! `prime-agent serve uhp` end to end: the real binary, an isolated HOME and
//! the scripted faux provider, over real HTTP. The conformance suite covers the
//! plain-text contract; these cover what it never sends: tools, budgets and
//! stops. What a child process inherits is tested in `pa-uhp`'s runner.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TOKEN: &str = "test-token-not-a-real-credential";

struct Server {
    child: Child,
    port: u16,
    home: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn command(home: &tempfile::TempDir, script: &Value) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .env("HOME", home.path())
        .env("PRIME_AGENT_CODING_AGENT_DIR", home.path().join("agent"))
        .env("PRIME_AGENT_FAUX_SCRIPT", script.to_string())
        .env("PRIME_AGENT_UHP_KEY", TOKEN)
        .env_remove("RLM_DEPTH")
        .current_dir(home.path());
    command
}

fn start(script: &Value) -> Server {
    let home = tempfile::TempDir::new().unwrap();
    let mut child = command(&home, script)
        .args(["serve", "uhp", "--port", "0"])
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary present");
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let (sender, receiver) = mpsc::channel();
    // Keeps reading after the port is known, so the server's log never fills the pipe.
    std::thread::spawn(move || {
        for line in lines.by_ref().map_while(Result::ok) {
            if let Some(address) = line.strip_prefix("UHP server listening on http://") {
                let port = address.rsplit(':').next().unwrap().parse::<u16>().unwrap();
                let _ = sender.send(port);
            }
        }
    });
    let port = receiver
        .recv_timeout(Duration::from_secs(60))
        .expect("server starts");
    Server { child, port, home }
}

/// HTTP/1.0, so a stream is delimited by the connection closing, not chunked.
fn request(server: &Server, method: &str, path: &str, body: Option<&Value>) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    let body = body.map(Value::to_string).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.0\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, payload) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, payload.to_string())
}

fn post_json(server: &Server, path: &str, body: &Value) -> (u16, Value) {
    let (status, payload) = request(server, "POST", path, Some(body));
    (status, serde_json::from_str(&payload).unwrap())
}

fn get_json(server: &Server, path: &str) -> (u16, Value) {
    let (status, payload) = request(server, "GET", path, None);
    (status, serde_json::from_str(&payload).unwrap())
}

fn stream_events(server: &Server, body: &Value) -> Vec<Value> {
    let (status, payload) = request(server, "POST", "/v1/responses", Some(body));
    assert_eq!(status, 200, "{payload}");
    payload
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

fn wait_for_status(server: &Server, id: &str, wanted: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, response) = get_json(server, &format!("/v1/responses/{id}"));
        if response["status"] == wanted {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "never reached {wanted}: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn text_of(response: &Value) -> Vec<String> {
    response["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "message")
        .map(|item| item["content"][0]["text"].as_str().unwrap().to_string())
        .collect()
}

fn slow_script() -> Value {
    json!({ "responses": [{ "content": [{ "type": "text", "text": "slow" }], "delayMs": 30000 }],
            "repeatLastResponse": true })
}

#[test]
fn a_task_with_tools_streams_every_item_and_reads_back_the_same() {
    let script = json!({ "responses": [
        { "content": [
            { "type": "thinking", "thinking": "let me look" },
            { "type": "toolCall", "name": "no_such_tool", "arguments": { "x": 1 } },
        ] },
        "all done",
    ] });
    let server = start(&script);
    let events = stream_events(&server, &json!({ "input": "go", "stream": true }));

    for (number, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], number, "{event}");
    }
    assert_eq!(events[0]["type"], "response.created");
    assert_eq!(events[1]["type"], "response.in_progress");
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "response.completed", "{terminal}");

    let response = &terminal["response"];
    assert_eq!(response["status"], "completed");
    let kinds: Vec<&str> = response["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "reasoning",
            "function_call",
            "function_call_output",
            "message"
        ]
    );
    let output = response["output"].as_array().unwrap();
    assert_eq!(output[0]["summary"][0]["text"], "let me look");
    assert_eq!(output[1]["name"], "no_such_tool");
    assert_eq!(output[1]["arguments"], r#"{"x":1}"#);
    assert_eq!(output[1]["call_id"], output[2]["call_id"]);
    assert_eq!(text_of(response), ["all done"]);

    // Every item that ended was announced first, in order.
    for item in output {
        let id = &item["id"];
        let position = |kind: &str| {
            events
                .iter()
                .position(|event| event["type"] == kind && event["item"]["id"] == *id)
        };
        let added = position("response.output_item.added").expect("added");
        let done = position("response.output_item.done").expect("done");
        assert!(added < done);
    }

    let id = response["id"].as_str().unwrap();
    let (status, stored) = get_json(&server, &format!("/v1/responses/{id}"));
    assert_eq!(status, 200);
    assert_eq!(&stored, response);
    let (_, items) = get_json(&server, &format!("/v1/responses/{id}/input_items"));
    assert_eq!(items["object"], "list");
    assert_eq!(items["data"][0]["content"][0]["text"], "go");
}

#[test]
fn a_continuation_resumes_the_same_prime_session_and_can_change_nothing_else() {
    let server = start(&json!({ "responses": ["one"], "repeatLastResponse": true }));
    let (_, first) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "first prompt" }),
    );
    let session = first["metadata"]["session_id"].as_str().unwrap();
    assert!(session.starts_with("hsess"));
    let record_path = server
        .home
        .path()
        .join("agent/uhp/sessions")
        .join(format!("{session}.json"));
    let record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    let prime_session = record["prime_session_id"].as_str().unwrap().to_string();

    let (status, second) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "second prompt", "previous_response_id": first["id"] }),
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["status"], "completed", "{second}");
    assert_eq!(second["metadata"]["session_id"], session);
    assert_eq!(second["previous_response_id"], first["id"]);
    let record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(record["prime_session_id"], prime_session.as_str());

    // Both prompts landed in one Prime Agent transcript, so context carried over.
    let mut found = false;
    let mut pending = vec![server.home.path().to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl")
                && path.to_string_lossy().contains(&prime_session)
            {
                let transcript = std::fs::read_to_string(&path).unwrap();
                found = transcript.contains("first prompt") && transcript.contains("second prompt");
            }
        }
    }
    assert!(found, "no transcript holds both prompts");

    let (status, missing) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "x", "previous_response_id": "resp_nosuchresponse" }),
    );
    assert_eq!(status, 404);
    assert_eq!(missing["error"]["code"], "response_not_found");
}

#[test]
fn cancelling_stops_the_work_and_leaves_the_session_continuable() {
    let server = start(&slow_script());
    let started_at = Instant::now();
    let (_, running) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "go", "background": true }),
    );
    assert_eq!(running["status"], "in_progress");
    let id = running["id"].as_str().unwrap();
    let session = running["metadata"]["session_id"].as_str().unwrap();

    let (status, busy) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "again", "previous_response_id": id }),
    );
    assert_eq!(status, 409, "{busy}");
    assert_eq!(busy["error"]["code"], "session_busy");

    std::thread::sleep(Duration::from_secs(1));
    let (status, _) = post_json(
        &server,
        &format!("/v1/sessions/{session}/cancel"),
        &json!({}),
    );
    assert_eq!(status, 200);
    let cancelled = wait_for_status(&server, id, "cancelled");
    assert!(cancelled["error"].is_null());
    assert!(
        started_at.elapsed() < Duration::from_secs(20),
        "the child ran on after the cancel"
    );

    // A retried cancel changes nothing.
    let (status, again) = post_json(&server, &format!("/v1/responses/{id}/cancel"), &json!({}));
    assert_eq!(status, 200);
    assert_eq!(again["status"], "cancelled");

    let (status, next) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "again", "previous_response_id": id, "background": true }),
    );
    assert_eq!(status, 200, "{next}");
    assert_eq!(next["metadata"]["session_id"], session);
    post_json(
        &server,
        &format!("/v1/responses/{}/cancel", next["id"].as_str().unwrap()),
        &json!({}),
    );
}

#[test]
fn a_dropped_stream_does_not_stop_the_task() {
    let script = json!({ "responses": [
        { "content": [{ "type": "text", "text": "finished anyway" }], "delayMs": 1500 },
    ] });
    let server = start(&script);
    let body = json!({ "input": "go", "stream": true }).to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    write!(
        stream,
        "POST /v1/responses HTTP/1.0\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reader = BufReader::new(stream);
    let mut id = None;
    let mut line = String::new();
    while id.is_none() {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if let Some(data) = line.strip_prefix("data: ") {
            let event: Value = serde_json::from_str(data.trim()).unwrap();
            id = event["response"]["id"].as_str().map(String::from);
        }
    }
    drop(reader);
    let done = wait_for_status(&server, &id.unwrap(), "completed");
    assert_eq!(text_of(&done), ["finished anyway"]);
}

#[test]
fn budgets_end_a_task_incomplete_with_their_reason() {
    let server = start(&slow_script());
    let (status, timed_out) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "go", "timeout_seconds": 1 }),
    );
    assert_eq!(status, 200);
    assert_eq!(timed_out["status"], "incomplete", "{timed_out}");
    assert_eq!(timed_out["incomplete_details"]["reason"], "timeout");

    let looping = json!({ "responses": [
        { "content": [{ "type": "toolCall", "name": "no_such_tool", "arguments": {} }] },
    ], "repeatLastResponse": true });
    let server = start(&looping);
    let (status, limited) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "go", "max_step": 2 }),
    );
    assert_eq!(status, 200);
    assert_eq!(limited["status"], "incomplete", "{limited}");
    assert_eq!(limited["incomplete_details"]["reason"], "max_steps");
    let calls = limited["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call")
        .count();
    assert!((2..=3).contains(&calls), "{calls} calls");
}

#[test]
fn a_provider_failure_is_a_failed_response_with_no_provider_text() {
    let script = json!({ "responses": [{
        "content": [],
        "stopReason": "error",
        "errorMessage": "bad key sk-secret at /Users/someone",
    }], "repeatLastResponse": true });
    let server = start(&script);
    let (status, response) = post_json(&server, "/v1/responses", &json!({ "input": "go" }));
    assert_eq!(status, 200);
    assert_eq!(response["status"], "failed", "{response}");
    assert_eq!(response["error"]["code"], "provider_error");
    assert!(!response.to_string().contains("sk-secret"));
}

#[test]
fn an_unavailable_model_is_refused_before_anything_runs() {
    let server = start(&json!({ "responses": ["ok"] }));
    let (status, refused) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "go", "model": "no-such-model" }),
    );
    assert_eq!(status, 422);
    assert_eq!(refused["error"]["code"], "model_unavailable");
    assert_eq!(refused["error"]["param"], "model");

    let (status, refused) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "go", "metadata": { "harness_id": "chrn_nope" } }),
    );
    assert_eq!(status, 404);
    assert_eq!(refused["error"]["code"], "harness_not_found");
}

#[test]
fn store_false_is_not_kept_and_keeps_no_workspace() {
    let server = start(&json!({ "responses": ["ok"], "repeatLastResponse": true }));
    let (_, response) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "private", "store": false }),
    );
    assert_eq!(response["status"], "completed", "{response}");
    assert_eq!(response["store"], false);
    let id = response["id"].as_str().unwrap();
    let (status, _) = get_json(&server, &format!("/v1/responses/{id}"));
    assert_eq!(status, 404);
    let state = server.home.path().join("agent/uhp");
    assert_eq!(
        std::fs::read_dir(state.join("responses")).unwrap().count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(state.join("sessions")).unwrap().count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(state.join("workspaces")).unwrap().count(),
        0
    );
}

#[test]
fn stopping_the_server_settles_a_running_task_as_interrupted() {
    let server = start(&slow_script());
    let (_, running) = post_json(
        &server,
        "/v1/responses",
        &json!({ "input": "go", "background": true }),
    );
    let id = running["id"].as_str().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let status = Command::new("kill")
        .args(["-TERM", &server.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let mut server = server;
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the server did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    let path = server
        .home
        .path()
        .join("agent/uhp/responses")
        .join(format!("{id}.json"));
    let record: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(record["response"]["status"], "incomplete");
    assert_eq!(
        record["response"]["incomplete_details"]["reason"],
        "interrupted"
    );
}

#[test]
fn the_server_refuses_to_start_unsafely() {
    let home = tempfile::TempDir::new().unwrap();
    let script = json!({ "responses": ["ok"] });
    let output = command(&home, &script)
        .args(["serve", "uhp", "--port", "0"])
        .env_remove("PRIME_AGENT_UHP_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PRIME_AGENT_UHP_KEY"));

    let output = command(&home, &script)
        .args(["serve", "uhp", "--port", "0", "--host", "0.0.0.0"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--allow-remote"));
}

#[test]
fn requests_need_the_token_and_a_version_the_server_speaks() {
    let server = start(&json!({ "responses": ["ok"] }));
    let raw = |request: String| {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    };
    let denied = raw("GET /v1/harnesses HTTP/1.0\r\n\r\n".to_string());
    assert!(denied.starts_with("HTTP/1.0 401"), "{denied}");
    assert!(denied.contains("missing_credential"));
    assert!(denied.to_lowercase().contains("uhp-version: 2026-10-19"));
    let wrong = raw("GET /v1/harnesses HTTP/1.0\r\nAuthorization: Bearer nope\r\n\r\n".to_string());
    assert!(wrong.contains("invalid_credential"), "{wrong}");
    let old = raw("GET /v1/uhp HTTP/1.0\r\nUHP-Version: 1999-01-01\r\n\r\n".to_string());
    assert!(old.starts_with("HTTP/1.0 400"), "{old}");
    assert!(old.contains("unsupported_protocol_version"));
    let open = raw("GET /v1/uhp HTTP/1.0\r\n\r\n".to_string());
    assert!(open.starts_with("HTTP/1.0 200"), "{open}");
}
