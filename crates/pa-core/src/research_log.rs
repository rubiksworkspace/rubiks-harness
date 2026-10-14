//! The research log: a local, append-only record of each run for studying
//! how the harness behaves. Every record carries the same join keys
//! (`run_id`, `session_id`, `acp_session_id`, `step`, `harness_version`) and
//! an `event_type`:
//!
//! - `run.start`: harness version, upstream changes, model, model settings,
//!   pricing, work context and participant (once per run).
//! - `user.prompt`, `context.built`, `api.call`, `api.retry`: each prompt,
//!   the context of each model call, and the call itself (request, reasoning,
//!   tokens, latency, HTTP status, parse status).
//! - `tool.result`, `error.detected`, `correction.delivered`, `verification`,
//!   `context.compacted`: the harness decisions in between.
//! - `acp.session_bound`: which IDE (ACP) session drives this session.
//!
//! Records go to `<agent dir>/research/<session id>.jsonl`; large content
//! (payloads, reasoning, tool output, summaries) is stored once under
//! `research/blobs/` and referenced by SHA-256. Secrets are stripped before
//! anything is written, and content is kept only with `content` consent:
//! the default `metadata` consent keeps sizes, hashes, counts and timings.
//! Nothing here ever leaves the machine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::Serialize;
use serde_json::{json, Value};

use crate::settings::types::ResearchSettings;

mod acp;
mod capture;
mod redact;
mod run_info;
mod store;

pub use acp::AcpTap;
pub use capture::{instrument_stream_fn, observe_agent_events};
pub use run_info::RunInfo;
pub use store::{acp_stream_path, blob_path, ide_log_path, research_dir, session_log_path};

use store::Store;

/// The harness commit this binary was built from.
pub const HARNESS_COMMIT: &str = env!("PA_HARNESS_COMMIT");

/// How much a participant agreed to have kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Consent {
    /// Sizes, hashes, counts and timings only.
    Metadata,
    /// Also the text itself: prompts, payloads, reasoning, tool output.
    Content,
}

/// The resolved `research` settings, environment first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchConfig {
    pub enabled: bool,
    pub consent: Consent,
    pub participant_id: Option<String>,
    pub task: Option<String>,
    /// The discounted window in UTC, as minutes after midnight.
    pub off_peak_utc: Option<(u32, u32)>,
}

impl ResearchConfig {
    /// `PRIME_AGENT_RESEARCH` (0/1), `PRIME_AGENT_RESEARCH_CONSENT`,
    /// `PRIME_AGENT_PARTICIPANT` and `PRIME_AGENT_TASK` override the settings;
    /// unset, the log is on.
    #[must_use]
    pub fn resolve(settings: &ResearchSettings) -> Self {
        Self::resolve_with(settings, |name| std::env::var(name).ok())
    }

    fn resolve_with(settings: &ResearchSettings, env: impl Fn(&str) -> Option<String>) -> Self {
        let enabled = match env("PRIME_AGENT_RESEARCH").as_deref().map(str::trim) {
            Some("0" | "false" | "no" | "off") => false,
            Some("1" | "true" | "yes" | "on") => true,
            // `PRIME_AGENT_RESEARCH_DEFAULT=off` (set for this repo's
            // tests) only changes what unset settings mean.
            _ => settings.enabled.unwrap_or_else(|| {
                env("PRIME_AGENT_RESEARCH_DEFAULT")
                    .as_deref()
                    .map(str::trim)
                    != Some("off")
            }),
        };
        let consent = env("PRIME_AGENT_RESEARCH_CONSENT")
            .or_else(|| settings.consent.clone())
            .is_some_and(|consent| consent.trim().eq_ignore_ascii_case("content"));
        let non_empty = |value: Option<String>| {
            value
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        Self {
            enabled,
            consent: if consent {
                Consent::Content
            } else {
                Consent::Metadata
            },
            participant_id: non_empty(
                env("PRIME_AGENT_PARTICIPANT").or_else(|| settings.participant_id.clone()),
            ),
            task: non_empty(env("PRIME_AGENT_TASK").or_else(|| settings.task.clone())),
            off_peak_utc: settings.off_peak_utc.as_deref().and_then(parse_window),
        }
    }
}

/// `HH:MM-HH:MM` as minutes after midnight.
fn parse_window(text: &str) -> Option<(u32, u32)> {
    let minutes = |part: &str| {
        let (hours, minutes) = part.trim().split_once(':')?;
        let (hours, minutes): (u32, u32) = (hours.parse().ok()?, minutes.parse().ok()?);
        (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
    };
    let (start, end) = text.split_once('-')?;
    Some((minutes(start)?, minutes(end)?))
}

/// Whether `minute` (after UTC midnight) falls in `window`, which may wrap
/// past midnight.
fn in_window((start, end): (u32, u32), minute: u32) -> bool {
    if start <= end {
        (start..end).contains(&minute)
    } else {
        minute >= start || minute < end
    }
}

/// One run of one session: the join keys and the store its records go to.
pub struct Recorder {
    store: Arc<Store>,
    config: ResearchConfig,
    run_id: String,
    session_id: String,
    harness_version: String,
    /// The latest model call's number (1-based; 0 before the first call).
    step: AtomicU64,
    decisions: Mutex<capture::Decisions>,
    /// The run's working directory, and the commit it started on (known
    /// once `run.start` has asked git).
    cwd: PathBuf,
    start_commit: Mutex<Option<String>>,
}

impl Recorder {
    /// Start recording a run and write its `run.start` record; `None` when
    /// the research log is turned off.
    #[must_use]
    pub fn start(
        agent_dir: &Path,
        session_id: &str,
        config: ResearchConfig,
        info: RunInfo,
    ) -> Option<Arc<Recorder>> {
        if !config.enabled {
            return None;
        }
        let card = info.card();
        let harness_version = format!("{HARNESS_COMMIT}+card.{}", &card.sha256[..12]);
        let recorder = Arc::new(Recorder {
            store: Store::for_agent_dir(agent_dir),
            config,
            run_id: uuid::Uuid::now_v7().to_string(),
            session_id: session_id.to_string(),
            harness_version,
            step: AtomicU64::new(0),
            decisions: Mutex::default(),
            cwd: info.cwd.clone(),
            start_commit: Mutex::new(None),
        });
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string(), Arc::downgrade(&recorder));
        // The repository facts come from `git`; asking it never holds up
        // the session's start.
        let started = Arc::clone(&recorder);
        let spawned = std::thread::Builder::new()
            .name("research-run-start".to_string())
            .spawn(move || {
                let (data, content) = info.run_start(&started, &card);
                *started
                    .start_commit
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    data["work"]["repo"]["commit"].as_str().map(str::to_string);
                started.record("run.start", None, data, content);
            });
        if let Err(error) = spawned {
            tracing::debug!(%error, "research run.start not recorded");
        }
        Some(recorder)
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    #[must_use]
    pub fn harness_version(&self) -> &str {
        &self.harness_version
    }

    fn keeps_content(&self) -> bool {
        self.config.consent == Consent::Content
    }

    fn current_step(&self) -> u64 {
        self.step.load(Ordering::SeqCst)
    }

    fn next_step(&self) -> u64 {
        self.step.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Whether this moment is in the configured off-peak window.
    fn off_peak_now(&self) -> Option<bool> {
        let window = self.config.off_peak_utc?;
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Some(in_window(window, ((seconds / 60) % (24 * 60)) as u32))
    }

    /// Queue one record. `step` defaults to the current step.
    fn record(
        &self,
        event_type: &str,
        step: Option<u64>,
        data: Value,
        content: Vec<(&'static str, String)>,
    ) {
        let mut record = json!({
            "ts": now_iso(),
            "event_type": event_type,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "acp_session_id": Value::Null,
            "step": step.unwrap_or_else(|| self.current_step()),
            "harness_version": self.harness_version,
        });
        record["data"] = data;
        self.store
            .append(&self.session_id, record, content, self.keeps_content());
    }

    /// Wait until this run's queued records are written (tests and exits).
    pub fn flush(&self) -> bool {
        self.store.flush(std::time::Duration::from_secs(5))
    }
}

fn registry() -> &'static Mutex<HashMap<String, Weak<Recorder>>> {
    static RECORDERS: OnceLock<Mutex<HashMap<String, Weak<Recorder>>>> = OnceLock::new();
    RECORDERS.get_or_init(Mutex::default)
}

/// The live recorder of `session_id` in this process, if any.
#[must_use]
pub fn recorder_for(session_id: &str) -> Option<Arc<Recorder>> {
    let mut recorders = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let recorder = recorders.get(session_id).and_then(Weak::upgrade);
    if recorder.is_none() {
        recorders.remove(session_id);
    }
    recorder
}

fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    crate::session::manager::format_iso(i64::try_from(now.as_millis()).unwrap_or(i64::MAX))
}

/// Every session's research log file, sorted by name.
#[must_use]
pub fn session_logs(agent_dir: &Path) -> Vec<PathBuf> {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(research_dir(agent_dir))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        // A session's ACP frame stream sits beside its log, not as a log.
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.ends_with(".jsonl") && !name.ends_with(".acp.jsonl")
            })
        })
        .collect();
    logs.sort();
    logs
}

#[cfg(test)]
mod tests;
