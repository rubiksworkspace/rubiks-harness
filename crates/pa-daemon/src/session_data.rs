//! Read-only views of saved session data for `prime-agent data` and the Rubiks
//! IDE data viewer: the session list with text and date filters, each
//! session's records split into chat, thinking, tool, usage and event items,
//! and the on-disk data stores under the agent directory.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context as _, Result};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::session_store::{list_sessions, SessionInfo};

/// A saved session as the data views list it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    pub id: String,
    /// The id's last segment, as `prime-agent sessions` prints it.
    pub short_id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub title: String,
    /// UTC timestamps as recorded in the session file.
    pub created: String,
    pub modified: String,
    /// `provider/model` of the session's last model change.
    pub model: Option<String>,
    pub message_count: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
    pub parent_session_path: Option<String>,
    pub rlm_depth: u32,
}

impl SessionRow {
    fn from_info(info: &SessionInfo) -> Self {
        let title = info
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                info.first_message
                    .lines()
                    .find(|line| !line.trim().is_empty())
            })
            .map_or_else(
                || "(no messages)".to_string(),
                |text| truncate_chars(text.trim(), 80),
            );
        let usage = info.usage.as_ref();
        Self {
            short_id: info.id.rsplit('-').next().unwrap_or(&info.id).to_string(),
            id: info.id.clone(),
            path: info.path.clone(),
            cwd: info.cwd.clone(),
            title,
            created: info.created.clone(),
            modified: info.modified.clone(),
            model: info
                .model
                .as_ref()
                .map(|(provider, model)| format!("{provider}/{model}")),
            message_count: info.message_count,
            input_tokens: usage.map_or(0, |usage| usage.input_tokens),
            output_tokens: usage.map_or(0, |usage| usage.output_tokens),
            cost: usage.map_or(0.0, |usage| usage.cost),
            parent_session_path: info.parent_session_path.clone(),
            rlm_depth: info.rlm_depth,
        }
    }
}

/// Which sessions to list. Dates are UTC calendar days (`YYYY-MM-DD`),
/// compared with the day each session was created.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFilter {
    search: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

impl SessionFilter {
    /// # Errors
    ///
    /// Returns an error when a date is not `YYYY-MM-DD`.
    pub fn new(search: Option<&str>, since: Option<&str>, until: Option<&str>) -> Result<Self> {
        for date in [since, until].into_iter().flatten() {
            if !is_calendar_day(date) {
                bail!("dates must look like 2026-10-13, got {date:?}");
            }
        }
        Ok(Self {
            search: search
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_lowercase),
            since: since.map(str::to_string),
            until: until.map(str::to_string),
        })
    }

    fn matches(&self, info: &SessionInfo, row: &SessionRow) -> bool {
        let day = row.created.get(..10).unwrap_or_default();
        if self.since.as_deref().is_some_and(|since| day < since) {
            return false;
        }
        if self.until.as_deref().is_some_and(|until| day > until) {
            return false;
        }
        self.search.as_deref().is_none_or(|search| {
            [
                row.id.as_str(),
                row.title.as_str(),
                row.cwd.as_str(),
                row.model.as_deref().unwrap_or_default(),
                info.all_messages_text.as_str(),
            ]
            .iter()
            .any(|field| field.to_lowercase().contains(search))
        })
    }
}

/// Saved sessions matching `filter`, most recently modified first.
#[must_use]
pub fn list_session_rows(session_dir: &Path, filter: &SessionFilter) -> Vec<SessionRow> {
    list_sessions(session_dir)
        .iter()
        .filter_map(|info| {
            let row = SessionRow::from_info(info);
            filter.matches(info, &row).then_some(row)
        })
        .collect()
}

/// The session `query` names: a full id, its short id, an id prefix, or a
/// session file path.
///
/// # Errors
///
/// Returns an error when no session, or more than one, matches.
pub fn find_session(session_dir: &Path, query: &str) -> Result<SessionRow> {
    let query = query.trim();
    let rows = list_session_rows(session_dir, &SessionFilter::default());
    let matches: Vec<&SessionRow> = rows
        .iter()
        .filter(|row| {
            row.id == query
                || row.short_id == query
                || row.path == Path::new(query)
                || (query.len() >= 4 && (row.id.starts_with(query) || row.id.ends_with(query)))
        })
        .collect();
    match matches.as_slice() {
        [row] => Ok((*row).clone()),
        [] => {
            bail!("no saved session matches {query:?}; list them with `prime-agent data sessions`")
        }
        many => bail!(
            "{query:?} matches {} sessions ({}); use more of the id",
            many.len(),
            many.iter()
                .map(|row| row.short_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The kinds of items a session's records are split into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    /// User and assistant text.
    Chat,
    /// The model's reasoning.
    Thinking,
    /// Tool calls and their results.
    Tools,
    /// Tokens and cost of each model call.
    Usage,
    /// Every other record: model and settings changes, state, notes.
    Events,
    /// Each JSONL record exactly as stored.
    Raw,
}

impl ItemKind {
    pub const ALL: [ItemKind; 6] = [
        ItemKind::Chat,
        ItemKind::Thinking,
        ItemKind::Tools,
        ItemKind::Usage,
        ItemKind::Events,
        ItemKind::Raw,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Chat => "chat",
            ItemKind::Thinking => "thinking",
            ItemKind::Tools => "tools",
            ItemKind::Usage => "usage",
            ItemKind::Events => "events",
            ItemKind::Raw => "raw",
        }
    }
}

impl fmt::Display for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which items a session view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemFilter {
    /// Chat, thinking, tools, usage and events, in record order.
    All,
    Only(ItemKind),
}

impl FromStr for ItemFilter {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        if text == "all" {
            return Ok(ItemFilter::All);
        }
        ItemKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text)
            .map(ItemFilter::Only)
            .with_context(|| {
                format!(
                    "unknown kind {text:?}; use all, chat, thinking, tools, usage, events or raw"
                )
            })
    }
}

/// One item of a session's data.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataItem {
    /// 1-based line of the record in the session file.
    pub line: usize,
    pub timestamp: String,
    pub kind: ItemKind,
    pub label: String,
    pub text: String,
    /// The part of the record this item came from.
    pub data: Value,
}

/// How many items of each kind a session has, before filtering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct KindCounts {
    pub chat: usize,
    pub thinking: usize,
    pub tools: usize,
    pub usage: usize,
    pub events: usize,
    pub raw: usize,
}

impl KindCounts {
    fn add(&mut self, kind: ItemKind) {
        let count = match kind {
            ItemKind::Chat => &mut self.chat,
            ItemKind::Thinking => &mut self.thinking,
            ItemKind::Tools => &mut self.tools,
            ItemKind::Usage => &mut self.usage,
            ItemKind::Events => &mut self.events,
            ItemKind::Raw => &mut self.raw,
        };
        *count += 1;
    }
}

/// One session's data, filtered.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionData {
    pub session: SessionRow,
    pub counts: KindCounts,
    pub items: Vec<DataItem>,
}

/// Read `session`'s file and return the items `filter` and `search` select.
/// `search` matches item labels and text, ignoring case.
///
/// # Errors
///
/// Returns an error when the session file cannot be read.
pub fn session_data(
    session: &SessionRow,
    filter: ItemFilter,
    search: Option<&str>,
) -> Result<SessionData> {
    let content = std::fs::read_to_string(&session.path)
        .with_context(|| format!("reading {}", session.path.display()))?;
    let search = search
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_lowercase);
    let mut counts = KindCounts::default();
    let mut items = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let raw = raw_item(index + 1, &record);
        counts.add(ItemKind::Raw);
        let split = record_items(index + 1, &record);
        for item in &split {
            counts.add(item.kind);
        }
        let selected = match filter {
            ItemFilter::Only(ItemKind::Raw) => vec![raw],
            ItemFilter::Only(kind) => split.into_iter().filter(|item| item.kind == kind).collect(),
            ItemFilter::All => split,
        };
        items.extend(selected.into_iter().filter(|item| {
            search.as_deref().is_none_or(|search| {
                item.label.to_lowercase().contains(search)
                    || item.text.to_lowercase().contains(search)
            })
        }));
    }
    Ok(SessionData {
        session: session.clone(),
        counts,
        items,
    })
}

fn raw_item(line: usize, record: &Value) -> DataItem {
    DataItem {
        line,
        timestamp: timestamp_of(record),
        kind: ItemKind::Raw,
        label: record["type"].as_str().unwrap_or("record").to_string(),
        text: record.to_string(),
        data: record.clone(),
    }
}

fn record_items(line: usize, record: &Value) -> Vec<DataItem> {
    let timestamp = timestamp_of(record);
    let item = |kind, label: String, text: String, data: &Value| DataItem {
        line,
        timestamp: timestamp.clone(),
        kind,
        label,
        text,
        data: data.clone(),
    };
    if record["type"] != "message" {
        let mut label = record["type"].as_str().unwrap_or("record").to_string();
        if let Some(custom) = record["customType"].as_str() {
            label = format!("{label}: {custom}");
        }
        return vec![item(ItemKind::Events, label, event_text(record), record)];
    }
    let message = &record["message"];
    let blocks = message["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    match message["role"].as_str().unwrap_or_default() {
        "user" => vec![item(
            ItemKind::Chat,
            "user".to_string(),
            block_text(blocks),
            message,
        )],
        "assistant" => {
            let mut items = Vec::new();
            for block in blocks {
                match block["type"].as_str().unwrap_or_default() {
                    "thinking" => items.push(item(
                        ItemKind::Thinking,
                        "thinking".to_string(),
                        block["thinking"].as_str().unwrap_or_default().to_string(),
                        block,
                    )),
                    "text" => items.push(item(
                        ItemKind::Chat,
                        "assistant".to_string(),
                        block["text"].as_str().unwrap_or_default().to_string(),
                        block,
                    )),
                    "toolCall" => items.push(item(
                        ItemKind::Tools,
                        format!("tool call: {}", block["name"].as_str().unwrap_or("tool")),
                        serde_json::to_string_pretty(&block["arguments"]).unwrap_or_default(),
                        block,
                    )),
                    other => items.push(item(
                        ItemKind::Events,
                        format!("assistant {other}"),
                        block.to_string(),
                        block,
                    )),
                }
            }
            if message["usage"].is_object() {
                let model = format!(
                    "{}/{}",
                    message["provider"].as_str().unwrap_or("?"),
                    message["model"].as_str().unwrap_or("?")
                );
                items.push(item(
                    ItemKind::Usage,
                    format!("usage: {model}"),
                    usage_text(&message["usage"]),
                    &message["usage"],
                ));
            }
            items
        }
        "toolResult" => {
            let error = if message["isError"] == true {
                " (error)"
            } else {
                ""
            };
            vec![item(
                ItemKind::Tools,
                format!(
                    "tool result: {}{error}",
                    message["toolName"].as_str().unwrap_or("tool")
                ),
                block_text(blocks),
                message,
            )]
        }
        role => vec![item(
            ItemKind::Events,
            format!("message: {role}"),
            block_text(blocks),
            message,
        )],
    }
}

fn timestamp_of(record: &Value) -> String {
    record["timestamp"].as_str().unwrap_or_default().to_string()
}

fn block_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .map(|block| match block["type"].as_str() {
            Some("text") => block["text"].as_str().unwrap_or_default().to_string(),
            Some("image") => "[image]".to_string(),
            _ => block.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn usage_text(usage: &Value) -> String {
    let tokens = |key: &str| usage[key].as_u64().unwrap_or(0);
    format!(
        "input {} · output {} · cache read {} · cache write {} · cost ${:.6}",
        tokens("input"),
        tokens("output"),
        tokens("cacheRead"),
        tokens("cacheWrite"),
        usage["cost"]["total"].as_f64().unwrap_or(0.0)
    )
}

/// The record without the fields every record has, as compact JSON.
fn event_text(record: &Value) -> String {
    let Some(fields) = record.as_object() else {
        return record.to_string();
    };
    let rest: Map<String, Value> = fields
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "type" | "id" | "parentId" | "timestamp"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Value::Object(rest).to_string()
}

/// A directory or file under the agent directory that holds harness data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DataStore {
    pub name: &'static str,
    pub description: &'static str,
    pub path: PathBuf,
    pub files: u64,
    pub bytes: u64,
}

/// The harness's data stores and their sizes. Credentials (`auth.json`)
/// and settings are deliberately not listed.
#[must_use]
pub fn data_stores(agent_dir: &Path, session_dir: &Path) -> Vec<DataStore> {
    let stores: [(&'static str, &'static str, PathBuf); 9] = [
        (
            "sessions",
            "Every session: messages, tool calls, usage and events (JSONL)",
            session_dir.to_path_buf(),
        ),
        (
            "session-artifacts",
            "Large tool outputs and files kept outside the session files",
            agent_dir.join("session-artifacts"),
        ),
        ("logs", "Background service logs", agent_dir.join("logs")),
        (
            "rlm-ledger",
            "Sub-agent spawn and usage ledger",
            agent_dir.join("rlm-ledger"),
        ),
        (
            "harness",
            "Persistent harness memories and refinements",
            agent_dir.join("harness"),
        ),
        (
            "daemon-workers",
            "Session worker state for crash recovery",
            agent_dir.join("daemon-workers"),
        ),
        ("mcp", "MCP server state", agent_dir.join("mcp")),
        ("models", "Cached model catalog", agent_dir.join("models")),
        (
            "telemetry.jsonl",
            "Local telemetry mirror (only when telemetry is turned on)",
            agent_dir.join("telemetry.jsonl"),
        ),
    ];
    stores
        .into_iter()
        .map(|(name, description, path)| {
            let (files, bytes) = size_of(&path);
            DataStore {
                name,
                description,
                path,
                files,
                bytes,
            }
        })
        .collect()
}

fn size_of(path: &Path) -> (u64, u64) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if meta.is_file() {
        return (1, meta.len());
    }
    if !meta.is_dir() {
        return (0, 0);
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| size_of(&entry.path()))
        .fold((0, 0), |(files, bytes), (more_files, more_bytes)| {
            (files + more_files, bytes + more_bytes)
        })
}

fn is_calendar_day(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = r#"{"type":"session","version":3,"id":"01a11094-73f6-735e-856b-9e291328986e","timestamp":"2026-10-06T09:38:46.902Z","cwd":"/work/repo","rlmDepth":0}
{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-10-06T09:38:46.957Z","provider":"deepseek","modelId":"deepseek-v4-flash"}
{"type":"message","id":"u1","parentId":"m1","timestamp":"2026-10-06T11:27:46.515Z","message":{"role":"user","content":[{"type":"text","text":"List the files"}],"timestamp":1}}
{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-10-06T11:27:47.938Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Use ls."},{"type":"text","text":"Listing them."},{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}}],"provider":"deepseek","model":"deepseek-v4-flash","usage":{"input":10,"output":5,"cacheRead":100,"cacheWrite":0,"totalTokens":115,"cost":{"total":0.0001}},"stopReason":"toolUse","timestamp":2}}
{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-10-06T11:27:48.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"README.md"}],"isError":false,"timestamp":3}}
not json
"#;

    fn session_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join("01a11094-73f6-735e-856b-9e291328986e.jsonl"),
            SESSION,
        )
        .unwrap();
        dir
    }

    #[test]
    fn lists_sessions_with_search_and_dates() {
        let dir = session_dir();
        let all = list_session_rows(dir.path(), &SessionFilter::default());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].short_id, "9e291328986e");
        assert_eq!(all[0].title, "List the files");
        assert_eq!(all[0].model.as_deref(), Some("deepseek/deepseek-v4-flash"));

        let matching = |search, since, until| {
            list_session_rows(
                dir.path(),
                &SessionFilter::new(search, since, until).unwrap(),
            )
            .len()
        };
        assert_eq!(
            matching(Some("LISTING"), None, None),
            1,
            "searches message text, ignoring case"
        );
        assert_eq!(matching(Some("absent"), None, None), 0);
        assert_eq!(matching(None, Some("2026-10-06"), Some("2026-10-06")), 1);
        assert_eq!(matching(None, Some("2026-10-07"), None), 0);
        assert!(SessionFilter::new(None, Some("6 Oct"), None).is_err());
    }

    #[test]
    fn finds_a_session_by_short_id_or_prefix() {
        let dir = session_dir();
        assert_eq!(
            find_session(dir.path(), "9e291328986e").unwrap().short_id,
            "9e291328986e"
        );
        assert_eq!(
            find_session(dir.path(), "01a11094").unwrap().short_id,
            "9e291328986e"
        );
        assert!(find_session(dir.path(), "ffff0000").is_err());
    }

    #[test]
    fn splits_records_into_kinds() {
        let dir = session_dir();
        let session = find_session(dir.path(), "9e291328986e").unwrap();
        let all = session_data(&session, ItemFilter::All, None).unwrap();
        assert_eq!(
            all.counts,
            KindCounts {
                chat: 2,
                thinking: 1,
                tools: 2,
                usage: 1,
                events: 2,
                raw: 5,
            }
        );
        let labels: Vec<(ItemKind, &str)> = all
            .items
            .iter()
            .map(|item| (item.kind, item.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            [
                (ItemKind::Events, "session"),
                (ItemKind::Events, "model_change"),
                (ItemKind::Chat, "user"),
                (ItemKind::Thinking, "thinking"),
                (ItemKind::Chat, "assistant"),
                (ItemKind::Tools, "tool call: bash"),
                (ItemKind::Usage, "usage: deepseek/deepseek-v4-flash"),
                (ItemKind::Tools, "tool result: bash"),
            ]
        );

        let tools = session_data(&session, "tools".parse().unwrap(), Some("readme")).unwrap();
        assert_eq!(tools.items.len(), 1);
        assert_eq!(tools.items[0].text, "README.md");
        assert_eq!(tools.items[0].line, 5);

        let raw = session_data(&session, "raw".parse().unwrap(), None).unwrap();
        assert_eq!(raw.items.len(), 5, "the line that is not JSON is skipped");
        assert!("everything".parse::<ItemFilter>().is_err());
    }
}
