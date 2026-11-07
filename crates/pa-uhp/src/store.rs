//! Running tasks in memory, finished ones on disk, and which session is busy.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::protocol::{is_valid_id, new_id, ApiError};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// How a task ended, as the final response reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Completed,
    /// Stopped short: `reason` is `incomplete_details.reason`.
    Incomplete {
        reason: &'static str,
    },
    Cancelled,
    Failed {
        code: &'static str,
        message: String,
    },
}

/// What the runner measured, merged into the response when the task ends.
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub outcome: Outcome,
    pub usage: Value,
    /// The model that ran, when it differs from the one the response names.
    pub ran_model: Option<String>,
}

struct TaskState {
    response: Value,
    events: Vec<Value>,
    /// Finished items by `output_index`, which is the order of `output`.
    output: BTreeMap<u64, Value>,
    terminal: bool,
}

pub struct Task {
    pub id: String,
    pub session_id: String,
    input_items: Vec<Value>,
    store: bool,
    deleted: AtomicBool,
    interrupted: AtomicBool,
    pub cancel: CancellationToken,
    state: Mutex<TaskState>,
    /// Bumped on every event, so followers wake with no lost notification.
    progress: watch::Sender<u64>,
}

impl Task {
    #[must_use]
    pub fn response(&self) -> Value {
        let state = lock(&self.state);
        response_with_output(&state)
    }

    /// Whether the server, not a client, is what stopped this task.
    #[must_use]
    pub fn was_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    /// Publish events in order.
    pub fn publish(&self, events: Vec<Value>) {
        if events.is_empty() {
            return;
        }
        append_events(&mut lock(&self.state), events);
        self.progress.send_modify(|count| *count += 1);
    }

    /// Publish the terminal event. Appended under one lock with the flag, so a
    /// follower that sees the task terminal has also seen its last event.
    fn publish_terminal(&self, event: Value) {
        let mut state = lock(&self.state);
        append_events(&mut state, vec![event]);
        state.terminal = true;
        drop(state);
        self.progress.send_modify(|count| *count += 1);
    }

    /// Events from `from` on, and whether the last of them ends the stream.
    #[must_use]
    pub fn events_from(&self, from: usize) -> (Vec<Value>, bool) {
        let state = lock(&self.state);
        let events = state.events.get(from..).unwrap_or_default().to_vec();
        (events, state.terminal)
    }

    #[must_use]
    pub fn follow(&self) -> watch::Receiver<u64> {
        self.progress.subscribe()
    }

    pub async fn wait_terminal(&self) {
        let mut progress = self.follow();
        while !lock(&self.state).terminal {
            if progress.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Numbering happens here, under the lock, so `sequence_number` has no gaps
/// whatever the events are.
fn append_events(state: &mut TaskState, events: Vec<Value>) {
    for mut event in events {
        if event["type"] == "response.output_item.done" {
            if let Some(index) = event["output_index"].as_u64() {
                state.output.insert(index, event["item"].clone());
            }
        }
        event["sequence_number"] = json!(state.events.len());
        state.events.push(event);
    }
}

fn response_with_output(state: &TaskState) -> Value {
    let mut response = state.response.clone();
    response["output"] = Value::Array(state.output.values().cloned().collect());
    response
}

/// A session's durable facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    /// The Prime Agent session tasks resume; unset until the first task opens one.
    pub prime_session_id: Option<String>,
    pub workspace: PathBuf,
}

/// Inputs to [`Store::begin`].
pub struct NewTask {
    pub session_id: String,
    pub previous_response_id: Option<String>,
    pub model: String,
    pub store: bool,
    pub metadata: serde_json::Map<String, Value>,
    pub input_items: Vec<Value>,
}

pub struct Store {
    responses_dir: PathBuf,
    sessions_dir: PathBuf,
    workspaces_dir: PathBuf,
    live: Mutex<HashMap<String, Arc<Task>>>,
    /// Session id to the task running in it.
    busy: Mutex<HashMap<String, Arc<Task>>>,
}

/// Owner-only: stored tasks hold prompts and output.
fn create_private_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("could not create {}", path.display()))
}

fn write_atomic(path: &Path, value: &Value) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("tmp{}", new_id("")));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("could not create {}", tmp.display()))?;
    file.write_all(value.to_string().as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("could not write {}", path.display()))
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

impl Store {
    /// Opens the state directory and settles tasks a previous server left
    /// running: their child is gone, so they end `incomplete`.
    ///
    /// # Errors
    /// The state directory cannot be created.
    pub fn open(state_dir: &Path) -> Result<Self> {
        let store = Self {
            responses_dir: state_dir.join("responses"),
            sessions_dir: state_dir.join("sessions"),
            workspaces_dir: state_dir.join("workspaces"),
            live: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashMap::new()),
        };
        create_private_dir(state_dir)?;
        create_private_dir(&store.responses_dir)?;
        create_private_dir(&store.sessions_dir)?;
        create_private_dir(&store.workspaces_dir)?;
        store.settle_interrupted()?;
        Ok(store)
    }

    fn settle_interrupted(&self) -> Result<()> {
        for entry in std::fs::read_dir(&self.responses_dir)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(mut record) = read_json(&path) else {
                continue;
            };
            if record["response"]["status"] == "in_progress" {
                record["response"]["status"] = json!("incomplete");
                record["response"]["incomplete_details"] = json!({ "reason": "interrupted" });
                write_atomic(&path, &record)?;
            }
        }
        Ok(())
    }

    fn response_path(&self, id: &str) -> PathBuf {
        self.responses_dir.join(format!("{id}.json"))
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{id}.json"))
    }

    fn stored_record(&self, id: &str) -> Option<Value> {
        is_valid_id(id, "resp_")
            .then(|| read_json(&self.response_path(id)))
            .flatten()
    }

    /// A session's own directory, used when no shared workspace is configured.
    #[must_use]
    pub fn session_workspace(&self, session_id: &str) -> PathBuf {
        self.workspaces_dir.join(session_id)
    }

    /// # Errors
    /// The session record cannot be written.
    pub fn save_session(&self, session: &SessionRecord) -> Result<()> {
        write_atomic(
            &self.session_path(&session.id),
            &json!({
                "id": session.id,
                "prime_session_id": session.prime_session_id,
                "workspace": session.workspace,
            }),
        )
    }

    #[must_use]
    pub fn load_session(&self, id: &str) -> Option<SessionRecord> {
        if !is_valid_id(id, "hsess") {
            return None;
        }
        let record = read_json(&self.session_path(id))?;
        Some(SessionRecord {
            id: id.to_string(),
            prime_session_id: record["prime_session_id"].as_str().map(String::from),
            workspace: PathBuf::from(record["workspace"].as_str()?),
        })
    }

    /// Registers a task as running in its session, atomically with the busy
    /// check, and publishes its first two events.
    ///
    /// # Errors
    /// `session_busy` when the session already runs a task.
    pub fn begin(&self, new: NewTask) -> Result<Arc<Task>, ApiError> {
        let mut busy = lock(&self.busy);
        if busy.contains_key(&new.session_id) {
            return Err(ApiError::session_busy());
        }
        let id = new_id("resp_");
        let mut metadata = new.metadata;
        metadata.insert("session_id".to_string(), json!(new.session_id));
        let response = json!({
            "id": id,
            "object": "response",
            "created_at": now_seconds(),
            "status": "in_progress",
            "error": null,
            "incomplete_details": null,
            "previous_response_id": new.previous_response_id,
            "model": new.model,
            "output": [],
            "store": new.store,
            "usage": null,
            "metadata": metadata,
        });
        let task = Arc::new(Task {
            id: id.clone(),
            session_id: new.session_id.clone(),
            input_items: new.input_items,
            store: new.store,
            deleted: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            state: Mutex::new(TaskState {
                response: response.clone(),
                events: Vec::new(),
                output: BTreeMap::new(),
                terminal: false,
            }),
            progress: watch::channel(0).0,
        });
        busy.insert(new.session_id, Arc::clone(&task));
        lock(&self.live).insert(id, Arc::clone(&task));
        drop(busy);
        self.persist(&task, &response);
        task.publish(vec![
            json!({ "type": "response.created", "response": response }),
            json!({ "type": "response.in_progress", "response": response }),
        ]);
        Ok(task)
    }

    fn persist(&self, task: &Task, response: &Value) {
        if !task.store || task.deleted.load(Ordering::SeqCst) {
            return;
        }
        let record = json!({ "response": response, "input_items": task.input_items });
        if let Err(error) = write_atomic(&self.response_path(&task.id), &record) {
            eprintln!("uhp: could not store {}: {error:#}", task.id);
        }
    }

    /// Ends a task. The record is written and the session freed before the
    /// terminal event goes out: a client that reacts to that event by reading
    /// the response back, or by continuing the session, must find it settled.
    pub fn finish(&self, task: &Task, result: TaskResult) {
        let (event_type, final_response) = {
            let mut state = lock(&task.state);
            let response = &mut state.response;
            let (status, event_type) = match &result.outcome {
                Outcome::Completed => ("completed", "response.completed"),
                Outcome::Incomplete { reason } => {
                    response["incomplete_details"] = json!({ "reason": reason });
                    ("incomplete", "response.incomplete")
                }
                // The status, not the event name, is authoritative (Streaming §4).
                Outcome::Cancelled => ("cancelled", "response.failed"),
                Outcome::Failed { code, message } => {
                    response["error"] = json!({
                        "type": "harness_error",
                        "code": code,
                        "message": message,
                        "param": null,
                        "detail": null,
                    });
                    ("failed", "response.failed")
                }
            };
            response["status"] = json!(status);
            response["usage"] = result.usage;
            if let Some(model) = result.ran_model {
                let requested = response["model"].clone();
                response["metadata"]["requested_model"] = requested;
                response["metadata"]["model_fallback"] = json!(true);
                response["metadata"]["model_fallback_reason"] =
                    json!("the provider chain ran a different model than the one requested");
                response["model"] = json!(model);
            }
            let settled = response_with_output(&state);
            state.response = settled.clone();
            (event_type, settled)
        };
        self.persist(task, &final_response);
        lock(&self.busy).remove(&task.session_id);
        task.publish_terminal(json!({ "type": event_type, "response": final_response }));
        lock(&self.live).remove(&task.id);
    }

    #[must_use]
    pub fn live_task(&self, id: &str) -> Option<Arc<Task>> {
        lock(&self.live)
            .get(id)
            .filter(|task| !task.deleted.load(Ordering::SeqCst))
            .cloned()
    }

    #[must_use]
    pub fn read_response(&self, id: &str) -> Option<Value> {
        if let Some(task) = self.live_task(id) {
            return Some(task.response());
        }
        Some(self.stored_record(id)?["response"].take())
    }

    #[must_use]
    pub fn read_input_items(&self, id: &str) -> Option<Value> {
        if let Some(task) = self.live_task(id) {
            return Some(Value::Array(task.input_items.clone()));
        }
        Some(self.stored_record(id)?["input_items"].take())
    }

    /// Deletes a stored response. A running task keeps running: deletion and
    /// cancellation are different intentions. Its record just never comes back.
    #[must_use]
    pub fn delete_response(&self, id: &str) -> bool {
        let live = self.live_task(id);
        if let Some(task) = &live {
            task.deleted.store(true, Ordering::SeqCst);
        }
        let removed =
            is_valid_id(id, "resp_") && std::fs::remove_file(self.response_path(id)).is_ok();
        live.is_some() || removed
    }

    /// The task running in a session, if any.
    #[must_use]
    pub fn running_in_session(&self, session_id: &str) -> Option<Arc<Task>> {
        lock(&self.busy).get(session_id).cloned()
    }

    /// Stops every running task because the server is going away.
    pub fn interrupt_all(&self) {
        for task in lock(&self.live).values() {
            task.interrupted.store(true, Ordering::SeqCst);
            task.cancel.cancel();
        }
    }

    #[must_use]
    pub fn live_count(&self) -> usize {
        lock(&self.live).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_task(session: &str, store: bool) -> NewTask {
        NewTask {
            session_id: session.to_string(),
            previous_response_id: None,
            model: "m".to_string(),
            store,
            metadata: serde_json::Map::new(),
            input_items: vec![json!({ "role": "user", "content": "hi" })],
        }
    }

    fn result(outcome: Outcome) -> TaskResult {
        TaskResult {
            outcome,
            usage: Value::Null,
            ran_model: None,
        }
    }

    fn open_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn sequence_numbers_start_at_zero_without_gaps() {
        let (_dir, store) = open_store();
        let task = store.begin(new_task("hsessa", true)).unwrap();
        task.publish(vec![json!({ "type": "x" }), json!({ "type": "y" })]);
        store.finish(&task, result(Outcome::Completed));
        let (events, terminal) = task.events_from(0);
        let numbers: Vec<u64> = events
            .iter()
            .map(|event| event["sequence_number"].as_u64().unwrap())
            .collect();
        assert_eq!(numbers, [0, 1, 2, 3, 4]);
        assert_eq!(events[0]["type"], "response.created");
        assert_eq!(events[4]["type"], "response.completed");
        assert!(terminal);
    }

    #[test]
    fn a_session_runs_one_task_at_a_time_and_frees_on_finish() {
        let (_dir, store) = open_store();
        let task = store.begin(new_task("hsessa", true)).unwrap();
        assert_eq!(
            store.begin(new_task("hsessa", true)).err().unwrap().code,
            "session_busy"
        );
        assert!(store.begin(new_task("hsessb", true)).is_ok());
        store.finish(&task, result(Outcome::Cancelled));
        assert!(store.begin(new_task("hsessa", true)).is_ok());
    }

    #[test]
    fn output_keeps_item_order_and_survives_cancelling() {
        let (_dir, store) = open_store();
        let task = store.begin(new_task("hsessa", true)).unwrap();
        task.publish(vec![
            json!({ "type": "response.output_item.done", "output_index": 1, "item": { "id": "b" } }),
            json!({ "type": "response.output_item.done", "output_index": 0, "item": { "id": "a" } }),
        ]);
        assert_eq!(task.response()["output"][0]["id"], "a");
        store.finish(&task, result(Outcome::Cancelled));
        let stored = store.read_response(&task.id).unwrap();
        assert_eq!(stored["status"], "cancelled");
        assert_eq!(stored["output"][1]["id"], "b");
        assert!(stored["incomplete_details"].is_null());
    }

    #[test]
    fn a_response_with_store_false_is_gone_once_finished() {
        let (_dir, store) = open_store();
        let task = store.begin(new_task("hsessa", false)).unwrap();
        assert!(store.read_response(&task.id).is_some());
        store.finish(&task, result(Outcome::Completed));
        assert!(store.read_response(&task.id).is_none());
    }

    #[test]
    fn deleting_a_running_task_does_not_cancel_it_or_keep_its_record() {
        let (_dir, store) = open_store();
        let task = store.begin(new_task("hsessa", true)).unwrap();
        assert!(store.delete_response(&task.id));
        assert!(!task.cancel.is_cancelled());
        assert!(store.read_response(&task.id).is_none());
        store.finish(&task, result(Outcome::Completed));
        assert!(store.read_response(&task.id).is_none());
        assert!(!store.delete_response(&task.id));
    }

    #[test]
    fn a_task_left_running_by_a_previous_server_ends_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let store = Store::open(dir.path()).unwrap();
            store.begin(new_task("hsessa", true)).unwrap().id.clone()
        };
        let store = Store::open(dir.path()).unwrap();
        let response = store.read_response(&id).unwrap();
        assert_eq!(response["status"], "incomplete");
        assert_eq!(response["incomplete_details"]["reason"], "interrupted");
    }

    #[test]
    fn a_session_record_round_trips() {
        let (_dir, store) = open_store();
        let session = SessionRecord {
            id: "hsessa".to_string(),
            prime_session_id: Some("uuid".to_string()),
            workspace: PathBuf::from("/work"),
        };
        store.save_session(&session).unwrap();
        assert_eq!(store.load_session("hsessa"), Some(session));
        assert_eq!(store.load_session("hsess"), None);
        assert_eq!(store.load_session("../hsessa"), None);
    }
}
