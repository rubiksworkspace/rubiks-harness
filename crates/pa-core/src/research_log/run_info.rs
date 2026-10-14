//! The `run.start` record: what is fixed for the whole run.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use super::store::hex;
use super::{Recorder, HARNESS_COMMIT};

/// The changes this harness carries on top of upstream Prime Agent, each
/// tagged with the layer it touches; maintained by hand alongside the code.
const HARNESS_CHANGES: &str = include_str!("harness_changes.json");

/// What the engine knows about a run when it starts.
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub cwd: PathBuf,
    pub provider: String,
    pub model_id: String,
    pub context_window: u64,
    pub max_tokens: u64,
    pub thinking_level: String,
    pub service_tier: Option<String>,
    /// The model's catalog prices, per million tokens.
    pub prices: Value,
    pub system_prompt: String,
    /// Each tool's name, description and parameter schema.
    pub tools: Vec<Value>,
    pub skills: Vec<String>,
    /// Harness settings that change behavior (compaction, retries, …).
    pub harness_settings: Value,
    /// The per-model prompt additions, when the model has any.
    pub model_prompt_extras: Option<String>,
    pub rlm_depth: u32,
    /// The daemon's id for the session, when a daemon worker hosts it.
    pub daemon_session_id: Option<String>,
}

/// The harness card: everything that configures the harness for a run apart
/// from the code itself and the model. Any change gives a new version.
pub(super) struct Card {
    pub(super) sha256: String,
    pub(super) json: String,
}

impl RunInfo {
    pub(super) fn card(&self) -> Card {
        let card = json!({
            "tools": self.tools,
            "skills": self.skills,
            "settings": self.harness_settings,
            "modelPromptExtras": self.model_prompt_extras,
        });
        let json = serde_json::to_string(&card).unwrap_or_default();
        Card {
            sha256: hex(&Sha256::digest(json.as_bytes())),
            json,
        }
    }

    pub(super) fn run_start(
        &self,
        recorder: &Recorder,
        card: &Card,
    ) -> (Value, Vec<(&'static str, String)>) {
        let prices_json = serde_json::to_string(&self.prices).unwrap_or_default();
        let price_table_version = hex(&Sha256::digest(prices_json.as_bytes()));
        let upstream: Value = serde_json::from_str(HARNESS_CHANGES).unwrap_or(Value::Null);
        let data = json!({
            "daemonSessionId": self.daemon_session_id,
            "harness": {
                "version": recorder.harness_version,
                "commit": HARNESS_COMMIT,
                "cardSha256": card.sha256,
                "systemPromptSha256": hex(&Sha256::digest(self.system_prompt.as_bytes())),
                "systemPromptBytes": self.system_prompt.len(),
                "tools": self.tools.iter().filter_map(|tool| tool["name"].as_str()).collect::<Vec<_>>(),
                "skills": self.skills,
                "rlmDepth": self.rlm_depth,
            },
            "upstream": upstream,
            "model": {
                "provider": self.provider,
                "requested": self.model_id,
                "contextWindow": self.context_window,
                "maxTokens": self.max_tokens,
            },
            "modelSettings": {
                "thinkingLevel": self.thinking_level,
                "serviceTier": self.service_tier,
                "settings": self.harness_settings,
            },
            "pricing": {
                "pricesPerMillion": self.prices,
                "priceTableVersion": &price_table_version[..12],
                "offPeakWindowUtc": recorder.config.off_peak_utc.map(|(start, end)| {
                    format!("{:02}:{:02}-{:02}:{:02}", start / 60, start % 60, end / 60, end % 60)
                }),
                "offPeakNow": recorder.off_peak_now(),
            },
            "work": {
                "cwd": self.cwd.display().to_string(),
                "repo": repository(&self.cwd),
                "task": recorder.config.task,
            },
            "participant": {
                "id": recorder.config.participant_id,
                "consent": recorder.config.consent,
            },
        });
        let content = vec![
            ("card", card.json.clone()),
            ("systemPrompt", self.system_prompt.clone()),
        ];
        (data, content)
    }
}

/// The git repository `cwd` is in: remote (without credentials), branch,
/// commit and how many tracked files differ from it. `None` outside a repo.
fn repository(cwd: &Path) -> Value {
    let git = |args: &[&str]| git(cwd, args);
    let Some(commit) = git(&["rev-parse", "HEAD"]) else {
        return Value::Null;
    };
    let remote = git(&["remote", "get-url", "origin"]).map(|url| without_credentials(&url));
    let changed = git(&["status", "--porcelain", "--untracked-files=no"])
        .map_or(0, |status| status.lines().count());
    json!({
        "root": git(&["rev-parse", "--show-toplevel"]),
        "remote": remote,
        "branch": git(&["rev-parse", "--abbrev-ref", "HEAD"]),
        "commit": commit,
        "changedFiles": changed,
    })
}

/// `git -C cwd <args>`'s trimmed output, when it succeeds.
pub(super) fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `https://user:token@host/path` as `https://host/path`.
fn without_credentials(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(scheme_end), Some(at)) if at > scheme_end => {
            format!("{}{}", &url[..scheme_end + 3], &url[at + 1..])
        }
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_lose_their_credentials() {
        assert_eq!(
            without_credentials("https://someone:ghp_secret@github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
        assert_eq!(
            without_credentials("git@github.com:org/repo.git"),
            "git@github.com:org/repo.git"
        );
    }

    #[test]
    fn the_changes_file_names_its_upstream() {
        let changes: Value = serde_json::from_str(HARNESS_CHANGES).unwrap();
        assert!(changes["upstreamBase"]
            .as_str()
            .is_some_and(|base| base.len() == 40));
        assert!(changes["changes"]
            .as_array()
            .is_some_and(|changes| !changes.is_empty()));
    }
}
