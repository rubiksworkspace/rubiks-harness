//! Read-only views of the research log (`pa_core::research_log`) for
//! `prime-agent data research` and the Rubiks IDE data viewer: one row per
//! session that has a log, and each session's records, filtered by event
//! type and text, with the IDE's ACP session id filled in from its binding.
//! A session's view merges three files by time: the harness's own log, the
//! ACP frame stream (`acp.message`, shown only when asked for) and what the
//! Rubiks IDE recorded for its ACP session (diff review, code survival and
//! origin, the survey).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use serde::Serialize;
use serde_json::Value;

use crate::session_data::{list_session_rows, SessionFilter};

/// A log's records with their 1-based line numbers.
type Records = Vec<(usize, Value)>;

/// One session's research log, summarized.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResearchRow {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub path: PathBuf,
    pub acp_session_id: Option<String>,
    pub runs: usize,
    pub records: usize,
    pub model_calls: usize,
    pub errors: usize,
    pub corrections: usize,
    pub verifications: usize,
    /// Records the Rubiks IDE wrote for this session's ACP session.
    pub ide_records: usize,
    /// ACP frames in both directions.
    pub acp_frames: usize,
    pub cost: f64,
    pub harness_versions: Vec<String>,
    pub first: Option<String>,
    pub last: Option<String>,
    /// Logs written under the daemon's id for this session (the ACP side),
    /// merged into its view.
    #[serde(skip)]
    pub linked_logs: Vec<PathBuf>,
}

/// One record as the data views show it; the fields match the session data
/// items, so the IDE renders all of them the same way.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResearchItem {
    pub line: usize,
    pub timestamp: String,
    /// The record's `event_type`.
    pub kind: String,
    pub label: String,
    /// The whole record as indented JSON.
    pub text: String,
    pub data: Value,
}

/// One session's research records, filtered.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResearchData {
    pub session: ResearchRow,
    /// Records per event type, before filtering.
    pub counts: BTreeMap<String, usize>,
    pub items: Vec<ResearchItem>,
}

/// Every session with a research log, most recent first.
#[must_use]
pub fn research_rows(agent_dir: &Path, session_dir: &Path) -> Vec<ResearchRow> {
    let titles: HashMap<String, String> = list_session_rows(session_dir, &SessionFilter::default())
        .into_iter()
        .map(|row| (row.id, row.title))
        .collect();
    let logs: Vec<(String, PathBuf, Records)> = pa_core::research_log::session_logs(agent_dir)
        .into_iter()
        .filter_map(|path| {
            let id = path.file_stem()?.to_string_lossy().into_owned();
            let records = read_records(&path).ok()?;
            Some((id, path, records))
        })
        .collect();
    // The ACP side only knows the daemon's id for a session; the session's
    // own `run.start` names that id, which joins the two.
    let file_for_daemon: HashMap<String, String> = logs
        .iter()
        .flat_map(|(id, _, records)| {
            records.iter().filter_map(move |(_, record)| {
                (event_type(record) == "run.start")
                    .then(|| record["data"]["daemonSessionId"].as_str())
                    .flatten()
                    .filter(|daemon| *daemon != id)
                    .map(|daemon| (daemon.to_string(), id.clone()))
            })
        })
        .collect();
    let mut linked: HashMap<String, Vec<(PathBuf, Records)>> = HashMap::new();
    let mut own = Vec::new();
    for (id, path, records) in logs {
        match file_for_daemon.get(&id) {
            Some(file_id) => linked
                .entry(file_id.clone())
                .or_default()
                .push((path, records)),
            None => own.push((id, path, records)),
        }
    }
    let mut rows: Vec<ResearchRow> = own
        .into_iter()
        .map(|(id, path, mut records)| {
            let linked_logs = linked.remove(&id).unwrap_or_default();
            let linked_paths: Vec<PathBuf> =
                linked_logs.iter().map(|(path, _)| path.clone()).collect();
            for (_, linked_records) in linked_logs {
                records.extend(linked_records);
            }
            let mut row = summarize(&id, &path, &records, titles.get(&id));
            row.ide_records = row
                .acp_session_id
                .as_deref()
                .and_then(|acp| read_records(&ide_log(&path, acp)).ok())
                .map_or(0, |records| records.len());
            row.acp_frames = std::iter::once(&path)
                .chain(&linked_paths)
                .map(|log| count_lines(&acp_stream(log)))
                .sum();
            row.linked_logs = linked_paths;
            row
        })
        .collect();
    rows.sort_by(|a, b| b.last.cmp(&a.last));
    rows
}

/// The log `query` names: a session id, its short id, or an id prefix or
/// suffix of at least four characters.
///
/// # Errors
///
/// Returns an error when no log matches, or more than one does.
pub fn find_research(agent_dir: &Path, session_dir: &Path, query: &str) -> Result<ResearchRow> {
    let query = query.trim();
    let rows = research_rows(agent_dir, session_dir);
    let matches: Vec<&ResearchRow> = rows
        .iter()
        .filter(|row| {
            row.id == query
                || row.short_id == query
                || (query.len() >= 4 && (row.id.starts_with(query) || row.id.ends_with(query)))
        })
        .collect();
    match matches.as_slice() {
        [row] => Ok((*row).clone()),
        [] => {
            bail!("no research log matches {query:?}; list them with `prime-agent data research`")
        }
        many => bail!(
            "{query:?} matches {} research logs ({}); use more of the id",
            many.len(),
            many.iter()
                .map(|row| row.short_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `row`'s records whose event type starts with `kind` (`api` selects
/// `api.call` and `api.retry`; `None` or `all` selects everything) and whose
/// label or JSON contains `search`, ignoring case.
///
/// # Errors
///
/// Returns an error when the log cannot be read.
pub fn research_data(
    row: &ResearchRow,
    kind: Option<&str>,
    search: Option<&str>,
) -> Result<ResearchData> {
    let mut records = read_records(&row.path)?;
    for log in &row.linked_logs {
        records.extend(read_records(log).unwrap_or_default());
    }
    if let Some(acp) = row.acp_session_id.as_deref() {
        records.extend(read_records(&ide_log(&row.path, acp)).unwrap_or_default());
    }
    for log in std::iter::once(&row.path).chain(&row.linked_logs) {
        records.extend(read_records(&acp_stream(log)).unwrap_or_default());
    }
    // Stable: records of one file keep their order at equal times.
    records.sort_by(|(_, a), (_, b)| {
        a["ts"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["ts"].as_str().unwrap_or_default())
    });
    let mut counts = BTreeMap::new();
    for (_, record) in &records {
        *counts.entry(event_type(record).to_string()).or_insert(0) += 1;
    }
    let kind = kind.filter(|kind| *kind != "all");
    let search = search
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_lowercase);
    let items = records
        .into_iter()
        .filter(|(_, record)| {
            let event = event_type(record);
            match kind {
                // The frame stream is long: only shown when asked for.
                None => event != "acp.message",
                Some(kind) => {
                    event.starts_with(kind)
                        && (event != "acp.message" || kind.len() > "acp".len() || kind == "acp")
                }
            }
        })
        .map(|(line, mut record)| {
            if record["acp_session_id"].is_null() {
                if let Some(acp) = &row.acp_session_id {
                    record["acp_session_id"] = Value::String(acp.clone());
                }
            }
            ResearchItem {
                line,
                timestamp: record["ts"].as_str().unwrap_or_default().to_string(),
                kind: event_type(&record).to_string(),
                label: label(&record),
                text: serde_json::to_string_pretty(&record).unwrap_or_default(),
                data: record,
            }
        })
        .filter(|item| {
            search.as_deref().is_none_or(|search| {
                item.label.to_lowercase().contains(search)
                    || item.text.to_lowercase().contains(search)
            })
        })
        .collect();
    Ok(ResearchData {
        session: row.clone(),
        counts,
        items,
    })
}

/// A stored blob (request payload, reasoning, tool output, summary, …) by
/// its SHA-256 or a unique prefix of at least 8 characters.
///
/// # Errors
///
/// Returns an error when no stored blob matches: it was recorded without
/// content consent, or the hash is wrong.
pub fn read_blob(agent_dir: &Path, sha256: &str) -> Result<String> {
    let sha256 = sha256.trim().to_lowercase();
    if sha256.len() < 8
        || !sha256
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        bail!("give at least 8 hex characters of the SHA-256");
    }
    let full = pa_core::research_log::blob_path(agent_dir, &sha256);
    let path = if full.is_file() {
        full
    } else {
        let dir = full.parent().map(Path::to_path_buf).unwrap_or_default();
        let found: Vec<PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&sha256))
            })
            .collect();
        match found.as_slice() {
            [path] => path.clone(),
            [] => bail!(
                "no stored content for {sha256}; content is only kept with research consent \"content\""
            ),
            _ => bail!("{sha256} matches more than one blob; use more of the hash"),
        }
    };
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

/// The ACP frame stream beside a session log.
fn acp_stream(log: &Path) -> PathBuf {
    log.with_extension("acp.jsonl")
}

/// What the IDE recorded for `acp_session_id`, beside a session log.
fn ide_log(log: &Path, acp_session_id: &str) -> PathBuf {
    let research = log.parent().unwrap_or(Path::new("."));
    pa_core::research_log::ide_log_path(research.parent().unwrap_or(research), acp_session_id)
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

fn read_records(path: &Path) -> Result<Vec<(usize, Value)>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .enumerate()
        .filter_map(|(index, line)| Some((index + 1, serde_json::from_str(line).ok()?)))
        .collect())
}

fn event_type(record: &Value) -> &str {
    record["event_type"].as_str().unwrap_or("record")
}

fn summarize(
    id: &str,
    path: &Path,
    records: &[(usize, Value)],
    title: Option<&String>,
) -> ResearchRow {
    let count = |kind: &str| {
        records
            .iter()
            .filter(|(_, record)| event_type(record) == kind)
            .count()
    };
    let mut runs: Vec<&str> = records
        .iter()
        .filter_map(|(_, record)| record["run_id"].as_str())
        .collect();
    runs.sort_unstable();
    runs.dedup();
    let mut versions: Vec<String> = records
        .iter()
        .filter(|(_, record)| !record["run_id"].is_null())
        .filter_map(|(_, record)| record["harness_version"].as_str().map(str::to_string))
        .collect();
    versions.sort();
    versions.dedup();
    let timestamps: Vec<&str> = records
        .iter()
        .filter_map(|(_, record)| record["ts"].as_str())
        .collect();
    let short_id = id.rsplit('-').next().unwrap_or(id).to_string();
    ResearchRow {
        id: id.to_string(),
        title: title
            .cloned()
            .unwrap_or_else(|| format!("Session {short_id}")),
        short_id,
        path: path.to_path_buf(),
        acp_session_id: records
            .iter()
            .rev()
            .find_map(|(_, record)| record["acp_session_id"].as_str().map(str::to_string)),
        runs: runs.len(),
        records: records.len(),
        model_calls: count("api.call"),
        errors: count("error.detected"),
        corrections: count("correction.delivered"),
        verifications: count("verification"),
        ide_records: 0,
        acp_frames: 0,
        linked_logs: Vec::new(),
        cost: records
            .iter()
            .filter(|(_, record)| event_type(record) == "api.call")
            .filter_map(|(_, record)| record["data"]["usage"]["cost"].as_f64())
            // `sum` of no floats is -0.0, which prints as "$-0.0000".
            .fold(0.0, |total, cost| total + cost),
        harness_versions: versions,
        first: timestamps.iter().min().map(|ts| (*ts).to_string()),
        last: timestamps.iter().max().map(|ts| (*ts).to_string()),
    }
}

/// A one-line summary of a record.
fn label(record: &Value) -> String {
    let data = &record["data"];
    let text = |value: &Value| match value {
        Value::String(text) => text.clone(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    };
    match event_type(record) {
        "run.start" => format!(
            "run {} · {} · {}",
            record["run_id"]
                .as_str()
                .unwrap_or("?")
                .get(..8)
                .unwrap_or("?"),
            text(&data["model"]["requested"]),
            text(&data["harness"]["version"])
        ),
        "user.prompt" => format!("prompt · {} chars", text(&data["chars"])),
        "context.built" => format!(
            "context · {} messages · ~{} tokens{}",
            text(&data["messages"]),
            text(&data["estimatedTokens"]),
            if data["harnessDigest"] == true {
                " · digest"
            } else {
                ""
            }
        ),
        "api.call" => format!(
            "{} · cache {}/{} · out {} (reasoning {}) · {} ms · {}",
            text(&data["modelReturned"]),
            text(&data["usage"]["cacheHit"]),
            text(&data["usage"]["cacheMiss"]),
            text(&data["usage"]["output"]),
            text(&data["usage"]["reasoning"]),
            text(&data["latencyMs"]["total"]),
            text(&data["stopReason"])
        ),
        "api.retry" => format!(
            "retry {} · attempt {}",
            text(&data["phase"]),
            text(&data["attempt"])
        ),
        "tool.result" => format!(
            "{}{}{}",
            text(&data["tool"]),
            match &data["exitCode"] {
                Value::Null => String::new(),
                code => format!(" · exit {code}"),
            },
            if data["truncated"] == true {
                " · truncated"
            } else {
                ""
            }
        ),
        "verification" => format!(
            "{} {} · {}",
            text(&data["kind"]),
            if data["passed"] == true {
                "passed"
            } else {
                "failed"
            },
            text(&data["command"])
        ),
        "error.detected" => format!("{} · {}", text(&data["id"]), text(&data["type"])),
        "correction.delivered" => format!(
            "answers {} · {} · {} steps later",
            text(&data["answers"]),
            text(&data["form"]),
            text(&data["lagSteps"])
        ),
        "context.compacted" => format!(
            "{} → {} tokens · {} messages summarized",
            text(&data["tokensBefore"]),
            text(&data["tokensAfter"]),
            text(&data["messagesSummarized"])
        ),
        "acp.session_bound" => format!("ACP session {}", text(&record["acp_session_id"])),
        "acp.message" => format!(
            "{} {} {}{}",
            text(&data["direction"]),
            text(&data["method"]),
            match &data["update"] {
                Value::Null => String::new(),
                update => format!("{} ", text(update)),
            },
            format_args!("· {} bytes", text(&data["bytes"]))
        ),
        "acp.prompt" => format!(
            "prompt · {} chars · {} attachments",
            text(&data["chars"]),
            data["resources"].as_array().map_or(0, Vec::len)
        ),
        "acp.turn_end" => format!(
            "turn ended · {} · {} ms",
            text(&data["stopReason"]),
            text(&data["durationMs"])
        ),
        "steering" => format!(
            "{} · {} ms after the agent's last {}",
            text(&data["kind"]),
            text(&data["sinceLastUpdate"]["ms"]),
            text(&data["sinceLastUpdate"]["kind"])
        ),
        "permission" => format!(
            "{} → {} in {} ms",
            text(&data["toolCall"]["title"]),
            text(&data["decisionKind"]),
            text(&data["decisionMs"])
        ),
        "run.end" => format!(
            "run ended · {} calls · ${} · {} · committed {}",
            text(&data["modelCalls"]),
            text(&data["cost"]),
            text(&data["repo"]["diffStat"]),
            text(&data["repo"]["committed"])
        ),
        "diff.review" => format!(
            "{} · {} · +{} −{} · {} ms",
            text(&data["decision"]),
            text(&data["path"]),
            text(&data["linesAdded"]),
            text(&data["linesRemoved"]),
            text(&data["decideMs"])
        ),
        "diff.survival" => format!(
            "{} · {}/{} lines survive after {}",
            text(&data["path"]),
            text(&data["survivingLines"]),
            text(&data["acceptedLines"]),
            text(&data["checkpoint"])
        ),
        "code.origin" => format!(
            "{} · {} · at {}",
            text(&data["path"]),
            text(&data["origin"]),
            text(&data["checkpoint"])
        ),
        "survey" => format!(
            "survey · TLX {} · agency {}",
            text(&data["tlxMean"]),
            text(&data["senseOfAgency"])
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = r#"{"ts":"2026-10-14T10:00:00.000Z","event_type":"acp.session_bound","run_id":null,"session_id":"01a1-abc","acp_session_id":"acp-9","step":0,"harness_version":"abc","data":{}}
{"ts":"2026-10-14T10:00:01.000Z","event_type":"run.start","run_id":"run-1","session_id":"01a1-abc","acp_session_id":null,"step":0,"harness_version":"abc+card.1","data":{"model":{"requested":"deepseek-v4-flash"},"harness":{"version":"abc+card.1"}}}
{"ts":"2026-10-14T10:00:02.000Z","event_type":"api.call","run_id":"run-1","session_id":"01a1-abc","acp_session_id":null,"step":1,"harness_version":"abc+card.1","data":{"modelReturned":"deepseek-flash","usage":{"cacheHit":10,"cacheMiss":5,"output":3,"reasoning":2,"cost":0.5},"latencyMs":{"total":900},"stopReason":"stop"}}
{"ts":"2026-10-14T10:00:03.000Z","event_type":"error.detected","run_id":"run-1","session_id":"01a1-abc","acp_session_id":null,"step":1,"harness_version":"abc+card.1","data":{"id":"e1","type":"tool_error"}}
"#;

    #[test]
    fn summarizes_and_filters_a_log() {
        let agent = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let research = pa_core::research_log::research_dir(agent.path());
        std::fs::create_dir_all(&research).unwrap();
        std::fs::write(research.join("01a1-abc.jsonl"), LOG).unwrap();

        let row = find_research(agent.path(), sessions.path(), "abc").unwrap();
        assert_eq!(
            (row.runs, row.records, row.model_calls, row.errors, row.cost),
            (1, 4, 1, 1, 0.5)
        );
        assert_eq!(row.acp_session_id.as_deref(), Some("acp-9"));
        assert_eq!(row.harness_versions, ["abc+card.1"]);

        let api = research_data(&row, Some("api"), None).unwrap();
        assert_eq!(api.items.len(), 1);
        assert_eq!(
            api.items[0].label,
            "deepseek-flash · cache 10/5 · out 3 (reasoning 2) · 900 ms · stop"
        );
        assert_eq!(api.items[0].data["acp_session_id"], "acp-9");
        assert_eq!(api.counts.get("run.start"), Some(&1));
        let errors = research_data(&row, None, Some("TOOL_ERROR")).unwrap();
        assert_eq!(errors.items.len(), 1);
        assert!(read_blob(agent.path(), "deadbeef").is_err());

        // The IDE's records and the ACP frames join the view.
        let ide = pa_core::research_log::ide_log_path(agent.path(), "acp-9");
        std::fs::create_dir_all(ide.parent().unwrap()).unwrap();
        std::fs::write(
            &ide,
            r#"{"ts":"2026-10-14T10:00:04.000Z","event_type":"diff.review","acp_session_id":"acp-9","data":{"decision":"accept","path":"a.rs","linesAdded":2,"linesRemoved":1,"decideMs":5000}}
"#,
        )
        .unwrap();
        std::fs::write(
            research.join("01a1-abc.acp.jsonl"),
            r#"{"ts":"2026-10-14T10:00:01.500Z","event_type":"acp.message","data":{"direction":"in","method":"session/prompt","bytes":40}}
"#,
        )
        .unwrap();
        let row = find_research(agent.path(), sessions.path(), "abc").unwrap();
        assert_eq!((row.ide_records, row.acp_frames), (1, 1));
        let all = research_data(&row, None, None).unwrap();
        let kinds: Vec<&str> = all.items.iter().map(|item| item.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "acp.session_bound",
                "run.start",
                "api.call",
                "error.detected",
                "diff.review"
            ]
        );
        assert_eq!(all.items[4].label, "accept · a.rs · +2 −1 · 5000 ms");
        let acp = research_data(&row, Some("acp"), None).unwrap();
        let kinds: Vec<&str> = acp.items.iter().map(|item| item.kind.as_str()).collect();
        assert_eq!(kinds, ["acp.session_bound", "acp.message"]);
    }

    #[test]
    fn the_acp_side_joins_its_session_through_the_daemon_id() {
        let agent = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let research = pa_core::research_log::research_dir(agent.path());
        std::fs::create_dir_all(&research).unwrap();
        std::fs::write(
            research.join("01b2-xyz.jsonl"),
            r#"{"ts":"2026-10-14T10:00:01.000Z","event_type":"run.start","run_id":"run-2","session_id":"01b2-xyz","step":0,"harness_version":"v","data":{"daemonSessionId":"0000aaaa1111"}}
"#,
        )
        .unwrap();
        std::fs::write(
            research.join("0000aaaa1111.jsonl"),
            r#"{"ts":"2026-10-14T10:00:00.000Z","event_type":"acp.session_bound","session_id":"0000aaaa1111","acp_session_id":"acp-7","data":{}}
{"ts":"2026-10-14T10:00:02.000Z","event_type":"acp.prompt","session_id":"0000aaaa1111","acp_session_id":"acp-7","data":{"chars":4,"resources":[]}}
"#,
        )
        .unwrap();
        std::fs::write(
            research.join("0000aaaa1111.acp.jsonl"),
            "{\"ts\":\"2026-10-14T10:00:00.000Z\",\"event_type\":\"acp.message\",\"data\":{}}\n{\"ts\":\"2026-10-14T10:00:02.000Z\",\"event_type\":\"acp.message\",\"data\":{}}\n",
        )
        .unwrap();

        let rows = research_rows(agent.path(), sessions.path());
        assert_eq!(
            rows.len(),
            1,
            "the daemon-id log is not a session of its own"
        );
        let row = &rows[0];
        assert_eq!(
            (
                row.id.as_str(),
                row.acp_session_id.as_deref(),
                row.acp_frames,
                row.records
            ),
            ("01b2-xyz", Some("acp-7"), 2, 3)
        );
        let kinds: Vec<String> = research_data(row, None, None)
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.kind)
            .collect();
        assert_eq!(kinds, ["acp.session_bound", "run.start", "acp.prompt"]);
    }
}
