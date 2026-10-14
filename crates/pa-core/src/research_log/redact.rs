//! Secret stripping for the research log: every string written to a record
//! or a blob passes through [`redact`] first.

use std::sync::OnceLock;

use fancy_regex::{Captures, Regex};

/// Credentials with a recognizable shape, replaced whole.
const TOKEN_PATTERNS: [&str; 9] = [
    // OpenAI-style and DeepSeek API keys.
    r"\bsk-[A-Za-z0-9_\-]{16,}",
    // GitHub tokens.
    r"\bgh[pousr]_[A-Za-z0-9]{20,}",
    r"\bgithub_pat_[A-Za-z0-9_]{20,}",
    // AWS access key ids.
    r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    // Slack tokens.
    r"\bxox[abprs]-[A-Za-z0-9\-]{10,}",
    // Google API keys.
    r"\bAIza[0-9A-Za-z_\-]{35}",
    // JSON web tokens.
    r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
    // Private keys.
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    // Vault tokens.
    r"\bhv[sbr]\.[A-Za-z0-9_\-]{20,}",
];

/// `name = value` / `"name": "value"` / `Authorization: Bearer value` where
/// the name says it holds a secret: the name stays, the value goes.
const ASSIGNMENT_PATTERN: &str = r#"(?i)((?:"|')?[A-Za-z0-9_\-]*(?:api[_\-]?key|secret|token|passwd|password|credential)[A-Za-z0-9_\-]*(?:"|')?\s*[:=]\s*(?:"|')?)([^\s"',;]{6,})"#;
const BEARER_PATTERN: &str = r"(?i)(\bbearer\s+)([A-Za-z0-9._~+/=\-]{12,})";

pub(crate) const REDACTED: &str = "<redacted>";

struct Patterns {
    tokens: Vec<Regex>,
    assignment: Regex,
    bearer: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        tokens: TOKEN_PATTERNS
            .iter()
            .map(|pattern| Regex::new(pattern).expect("valid token pattern"))
            .collect(),
        assignment: Regex::new(ASSIGNMENT_PATTERN).expect("valid assignment pattern"),
        bearer: Regex::new(BEARER_PATTERN).expect("valid bearer pattern"),
    })
}

/// `text` with every recognizable secret replaced by `<redacted>`.
pub(crate) fn redact(text: &str) -> String {
    let patterns = patterns();
    let mut out = text.to_string();
    for pattern in &patterns.tokens {
        out = pattern.replace_all(&out, REDACTED).into_owned();
    }
    for pattern in [&patterns.assignment, &patterns.bearer] {
        out = pattern
            .replace_all(&out, |captures: &Captures<'_>| {
                if &captures[2] == REDACTED {
                    captures[0].to_string()
                } else {
                    format!("{}{REDACTED}", &captures[1])
                }
            })
            .into_owned();
    }
    out
}

/// Redact every string inside `value`, keys included.
pub(crate) fn redact_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = redact(text),
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_value),
        serde_json::Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut item) in entries {
                redact_value(&mut item);
                map.insert(redact(&key), item);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_keys_tokens_and_assignments() {
        let text = concat!(
            "key sk-abcdefghijklmnopqrstuvwx, gh ghp_0123456789abcdefghijABCDEFGHIJ, ",
            "export DEEPSEEK_API_KEY=abc123def456 and {\"password\": \"hunter22\"} ",
            "Authorization: Bearer abcdefghijklmnop.qrstu keep this"
        );
        assert_eq!(
            redact(text),
            concat!(
                "key <redacted>, gh <redacted>, ",
                "export DEEPSEEK_API_KEY=<redacted> and {\"password\": \"<redacted>\"} ",
                "Authorization: Bearer <redacted> keep this"
            )
        );
        assert_eq!(redact("tokens used: 120"), "tokens used: 120");
    }

    #[test]
    fn redacts_inside_json() {
        let mut value = serde_json::json!({"env": {"API_TOKEN": "x", "cmd": "curl -H 'Authorization: Bearer abcdefghijklmnop'"}});
        redact_value(&mut value);
        assert_eq!(
            value,
            serde_json::json!({"env": {"API_TOKEN": "x", "cmd": "curl -H 'Authorization: Bearer <redacted>'"}})
        );
    }
}
