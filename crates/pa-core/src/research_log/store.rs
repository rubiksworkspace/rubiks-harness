//! Where research records go: one append-only JSONL file per session under
//! `<agent dir>/research/`, and large content once per hash under
//! `research/blobs/`. A background thread does every write, so a capture
//! point only hands its record over and never waits on the disk.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use super::redact::{redact, redact_value};

/// The research log directory under the agent directory.
#[must_use]
pub fn research_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("research")
}

/// Where the blob with this SHA-256 (hex) lives.
#[must_use]
pub fn blob_path(agent_dir: &Path, sha256: &str) -> PathBuf {
    let prefix = sha256.get(..2).unwrap_or("00");
    research_dir(agent_dir)
        .join("blobs")
        .join(prefix)
        .join(sha256)
}

/// The record file of a session.
#[must_use]
pub fn session_log_path(agent_dir: &Path, session_id: &str) -> PathBuf {
    research_dir(agent_dir).join(format!("{}.jsonl", file_stem(session_id)))
}

/// The ACP frame stream of a session (every frame between the IDE and
/// Prime Agent, in order).
#[must_use]
pub fn acp_stream_path(agent_dir: &Path, session_id: &str) -> PathBuf {
    research_dir(agent_dir).join(acp_stream_file_name(session_id))
}

pub(crate) fn acp_stream_file_name(session_id: &str) -> String {
    format!("{}.acp.jsonl", file_stem(session_id))
}

/// What the Rubiks IDE records for one ACP session (diff review, code
/// survival and origin, the session survey).
#[must_use]
pub fn ide_log_path(agent_dir: &Path, acp_session_id: &str) -> PathBuf {
    research_dir(agent_dir)
        .join("ide")
        .join(format!("{}.jsonl", file_stem(acp_session_id)))
}

/// `id` with anything but letters, digits, `-` and `_` replaced, so it is a
/// safe file name.
fn file_stem(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

enum Job {
    Record {
        session_id: String,
        record: Value,
        /// `(field, text)` pairs stored as blobs and referenced from `data`.
        content: Vec<(&'static str, String)>,
        keep_content: bool,
    },
    /// A record for a file of its own under the research directory (the
    /// ACP frame stream), redacted whole.
    Line {
        file_name: String,
        record: Value,
    },
    Flush(mpsc::Sender<()>),
}

/// The writer for one agent directory, shared by every run in the process.
pub(crate) struct Store {
    sender: mpsc::Sender<Job>,
}

impl Store {
    /// The process-wide store for `agent_dir`, started on first use.
    pub(crate) fn for_agent_dir(agent_dir: &Path) -> Arc<Store> {
        static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<Store>>>> = OnceLock::new();
        let mut stores = STORES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            stores
                .entry(agent_dir.to_path_buf())
                .or_insert_with(|| Arc::new(Store::start(agent_dir.to_path_buf()))),
        )
    }

    fn start(agent_dir: PathBuf) -> Store {
        let (sender, receiver) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("research-log".to_string())
            .spawn(move || {
                for job in receiver {
                    match job {
                        Job::Record {
                            session_id,
                            record,
                            content,
                            keep_content,
                        } => {
                            if let Err(error) =
                                write_record(&agent_dir, &session_id, record, content, keep_content)
                            {
                                tracing::debug!(%error, "research log write failed");
                            }
                        }
                        Job::Line { file_name, record } => {
                            if let Err(error) = write_line(&agent_dir, &file_name, record) {
                                tracing::debug!(%error, "research log write failed");
                            }
                        }
                        Job::Flush(done) => {
                            // The flusher may have stopped waiting.
                            let _ = done.send(());
                        }
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::debug!(%error, "research log writer did not start");
        }
        Store { sender }
    }

    /// Queue one record. `content` holds the large or sensitive fields; each
    /// is stored as a blob (with content consent) and replaced in `data` by
    /// its hash and size.
    pub(crate) fn append(
        &self,
        session_id: &str,
        record: Value,
        content: Vec<(&'static str, String)>,
        keep_content: bool,
    ) {
        // A stopped writer only loses research records, never the session.
        let _ = self.sender.send(Job::Record {
            session_id: session_id.to_string(),
            record,
            content,
            keep_content,
        });
    }

    /// Queue `record` for `research/<file_name>`; every string is redacted.
    pub(crate) fn append_line(&self, file_name: String, record: Value) {
        // A stopped writer only loses research records, never the session.
        let _ = self.sender.send(Job::Line { file_name, record });
    }

    /// Wait (up to `timeout`) until every queued record is on disk.
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let (done, wait) = mpsc::channel();
        if self.sender.send(Job::Flush(done)).is_err() {
            return false;
        }
        wait.recv_timeout(timeout).is_ok()
    }
}

fn write_record(
    agent_dir: &Path,
    session_id: &str,
    mut record: Value,
    content: Vec<(&'static str, String)>,
    keep_content: bool,
) -> std::io::Result<()> {
    if let Some(data) = record.get_mut("data") {
        redact_value(data);
    }
    for (field, text) in content {
        let text = redact(&text);
        let sha256 = hex(&Sha256::digest(text.as_bytes()));
        if keep_content {
            let path = blob_path(agent_dir, &sha256);
            if !path.exists() {
                std::fs::create_dir_all(path.parent().unwrap_or(agent_dir))?;
                let partial = path.with_extension("partial");
                std::fs::write(&partial, text.as_bytes())?;
                std::fs::rename(&partial, &path)?;
            }
        }
        record["data"][field] = json!({
            "sha256": sha256,
            "bytes": text.len(),
            "stored": keep_content,
        });
    }
    append(&session_log_path(agent_dir, session_id), &record)
}

fn write_line(agent_dir: &Path, file_name: &str, mut record: Value) -> std::io::Result<()> {
    redact_value(&mut record);
    append(&research_dir(agent_dir).join(file_name), &record)
}

fn append(path: &Path, record: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap_or(path))?;
    let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    line.push(b'\n');
    // One write per record on an append-mode file: the ACP daemon and the
    // session worker can both write a session's log.
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&line)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
