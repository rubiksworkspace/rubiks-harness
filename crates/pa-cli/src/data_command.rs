//! `prime-agent data`: browse what the harness has saved. Lists sessions with
//! text and date filters, shows one session's chat, thinking, tool calls,
//! usage, events or raw records, and lists the data stores. `--json` output
//! is what the Rubiks IDE data viewer reads.

use pa_daemon::session_data::{
    data_stores, find_session, list_session_rows, session_data, DataItem, ItemFilter, SessionData,
    SessionFilter, SessionRow,
};

use crate::config::{get_agent_dir, APP_NAME};

/// Lines of an item's text shown without `--full`.
const PREVIEW_LINES: usize = 12;

/// The command's parsed arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct DataArgs {
    subcommand: Option<String>,
    session: Option<String>,
    search: Option<String>,
    since: Option<String>,
    until: Option<String>,
    kind: Option<String>,
    json: bool,
    full: bool,
}

fn parse_args(args: &[String]) -> Result<DataArgs, String> {
    let mut parsed = DataArgs::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = |flag: &str| {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--full" => parsed.full = true,
            "--search" | "-s" => parsed.search = Some(value("--search")?),
            "--since" => parsed.since = Some(value("--since")?),
            "--until" => parsed.until = Some(value("--until")?),
            "--kind" | "-k" => parsed.kind = Some(value("--kind")?),
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            word if parsed.subcommand.is_none() => parsed.subcommand = Some(word.to_string()),
            word if parsed.session.is_none() => parsed.session = Some(word.to_string()),
            word => return Err(format!("unexpected argument {word:?}")),
        }
    }
    Ok(parsed)
}

/// Run `prime-agent data`; returns the process exit code.
pub(crate) fn run_data_command(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    let agent_dir = get_agent_dir();
    let session_dir = match pa_daemon::paths::sessions_dir(&agent_dir) {
        Ok(dir) => dir,
        Err(error) => return fail(&format!("{error:#}")),
    };
    let result = match parsed.subcommand.as_deref() {
        None | Some("sessions") => run_sessions(&session_dir, &parsed),
        Some("show") => run_show(&session_dir, &parsed),
        Some("stores") => {
            let stores = data_stores(&agent_dir, &session_dir);
            if parsed.json {
                print_json(&stores)
            } else {
                for store in &stores {
                    println!(
                        "{:<18} {:>9}  {:>5} files  {}",
                        store.name,
                        human_bytes(store.bytes),
                        store.files,
                        store.description
                    );
                    println!("{:<18} {}", "", store.path.display());
                }
                Ok(())
            }
        }
        Some(other) => Err(format!(
            "unknown data command {other:?}; use sessions, show or stores (see `{APP_NAME} help data`)"
        )),
    };
    match result {
        Ok(()) => 0,
        Err(error) => fail(&error),
    }
}

fn run_sessions(session_dir: &std::path::Path, parsed: &DataArgs) -> Result<(), String> {
    if parsed.session.is_some() {
        return Err(format!("Usage: {APP_NAME} data sessions [--search TEXT] [--since DATE] [--until DATE] [--json]"));
    }
    let filter = SessionFilter::new(
        parsed.search.as_deref(),
        parsed.since.as_deref(),
        parsed.until.as_deref(),
    )
    .map_err(|error| format!("{error:#}"))?;
    let rows = list_session_rows(session_dir, &filter);
    if parsed.json {
        return print_json(&rows);
    }
    if rows.is_empty() {
        println!("No saved sessions match.");
        return Ok(());
    }
    println!(
        "{:<17} {:<12} {:<28} {:>5} {:>15} {:>8}  TITLE",
        "CREATED (UTC)", "ID", "MODEL", "MSGS", "TOKENS IN/OUT", "COST"
    );
    for row in &rows {
        println!(
            "{:<17} {:<12} {:<28} {:>5} {:>15} {:>8}  {}",
            utc_minutes(&row.created),
            row.short_id,
            clip(row.model.as_deref().unwrap_or("-"), 28),
            row.message_count,
            format!(
                "{}/{}",
                compact(row.input_tokens),
                compact(row.output_tokens)
            ),
            format!("${:.2}", row.cost),
            clip(&row.title, 60)
        );
    }
    println!("\n{} session(s). Details: {APP_NAME} data show <ID> [--kind chat|thinking|tools|usage|events|raw]", rows.len());
    Ok(())
}

fn run_show(session_dir: &std::path::Path, parsed: &DataArgs) -> Result<(), String> {
    let Some(query) = parsed.session.as_deref() else {
        return Err(format!(
            "Usage: {APP_NAME} data show <ID> [--kind KIND] [--search TEXT] [--full] [--json]"
        ));
    };
    let session = find_session(session_dir, query).map_err(|error| format!("{error:#}"))?;
    let filter: ItemFilter = parsed
        .kind
        .as_deref()
        .unwrap_or("all")
        .parse()
        .map_err(|error: anyhow::Error| format!("{error:#}"))?;
    let data = session_data(&session, filter, parsed.search.as_deref())
        .map_err(|error| format!("{error:#}"))?;
    if parsed.json {
        return print_json(&data);
    }
    print_session_data(&data, parsed.full);
    Ok(())
}

fn print_session_data(data: &SessionData, full: bool) {
    let SessionData {
        session,
        counts,
        items,
    } = data;
    print_header(session);
    println!(
        "Items: chat {} · thinking {} · tools {} · usage {} · events {} · raw records {}\n",
        counts.chat, counts.thinking, counts.tools, counts.usage, counts.events, counts.raw
    );
    if items.is_empty() {
        println!("No items match.");
        return;
    }
    for item in items {
        print_item(item, full);
    }
    if !full {
        println!("Long items are cut to {PREVIEW_LINES} lines; add --full to see everything.");
    }
}

fn print_header(session: &SessionRow) {
    println!("Session  {}  ({})", session.short_id, session.id);
    println!("Title    {}", session.title);
    println!("Folder   {}", session.cwd);
    println!(
        "Model    {}   created {} UTC, updated {} UTC",
        session.model.as_deref().unwrap_or("-"),
        utc_minutes(&session.created),
        utc_minutes(&session.modified)
    );
    println!(
        "Usage    {} in / {} out tokens, ${:.4}",
        session.input_tokens, session.output_tokens, session.cost
    );
    println!("File     {}", session.path.display());
}

fn print_item(item: &DataItem, full: bool) {
    let time = item.timestamp.get(11..19).unwrap_or("--:--:--");
    println!(
        "[{time}] {:<8} {}  (line {})",
        item.kind.as_str(),
        item.label,
        item.line
    );
    let lines: Vec<&str> = item.text.lines().collect();
    let shown = if full {
        lines.len()
    } else {
        lines.len().min(PREVIEW_LINES)
    };
    for line in &lines[..shown] {
        println!(
            "    {}",
            if full {
                (*line).to_string()
            } else {
                clip(line, 200)
            }
        );
    }
    if shown < lines.len() {
        println!("    … {} more lines", lines.len() - shown);
    }
    println!();
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    println!("{text}");
    Ok(())
}

fn fail(message: &str) -> i32 {
    eprintln!("Error: {message}");
    1
}

/// `2026-10-06T09:38:46.902Z` as `2026-10-06 09:38`.
fn utc_minutes(timestamp: &str) -> String {
    match (timestamp.get(..10), timestamp.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time}"),
        _ => timestamp.to_string(),
    }
}

fn compact(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.1}m", tokens as f64 / 1_000_000.0),
    }
}

fn human_bytes(bytes: u64) -> String {
    match bytes {
        0..1_024 => format!("{bytes} B"),
        1_024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1_024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_subcommands_and_filters() {
        assert_eq!(
            parse_args(&strings(&[
                "show",
                "9e291328986e",
                "--kind",
                "tools",
                "-s",
                "readme",
                "--json"
            ]))
            .unwrap(),
            DataArgs {
                subcommand: Some("show".to_string()),
                session: Some("9e291328986e".to_string()),
                kind: Some("tools".to_string()),
                search: Some("readme".to_string()),
                json: true,
                ..DataArgs::default()
            }
        );
        assert_eq!(
            parse_args(&strings(&["--since"])).unwrap_err(),
            "--since requires a value"
        );
        assert_eq!(
            parse_args(&strings(&["--colour"])).unwrap_err(),
            "unknown option --colour"
        );
    }

    #[test]
    fn formats_sizes_and_times() {
        assert_eq!(utc_minutes("2026-10-06T09:38:46.902Z"), "2026-10-06 09:38");
        assert_eq!(compact(1_234_567), "1.2m");
        assert_eq!(human_bytes(5_970_000), "5.7 MB");
    }
}
