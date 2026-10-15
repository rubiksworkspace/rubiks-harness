#!/usr/bin/env python3
"""Merge the per-task results of a SWE-bench Pro run into one table.

Each task directory (from run_task.sh) holds result.json, meta.json, the
harness's data dir (prime-agent/: research log, sessions, ...), the egress
log and the verifier output. This writes, into the output directory:

  summary.csv    one row per task: the verdict next to the research-log
                 metrics (model calls, tokens, cache, latency, errors,
                 corrections, verification, compaction, cost) and the
                 anti-cheating signals
  tasks.jsonl    the same rows as JSON, with the suspicious commands listed
  summary.md     the headline numbers, per repository (also printed)

Usage: summarize.py <results dir> <output dir>
"""
import csv
import json
import re
import statistics
import sys
from pathlib import Path

# Commands that can reach for the answer instead of deriving it: history
# lookups and network fetches (the agent sandbox has no network, so these
# should fail, but trying is itself the signal).
SUSPICIOUS = [
    ("git_history", re.compile(r"\bgit\s+(log|show|reflog|rev-list|cat-file|blame|fsck)\b")),
    ("git_network", re.compile(r"\bgit\s+(fetch|pull|clone|remote\s+add)\b")),
    ("http_fetch", re.compile(r"\b(curl|wget)\b|urllib|requests\.get|httpx")),
    ("package_install", re.compile(r"\b(pip3?|uv\s+pip|npm|yarn|pnpm)\s+(install|add)\b|\bgo\s+(get|install)\b")),
]

COLUMNS = [
    "instance_id", "repo", "language", "resolved", "resolved_official", "required_tests",
    "required_tests_passed", "patch_differs_from_official", "ignored_files_added",
    "runtime_files_dropped", "skill_install_failures",
    "agent_seconds", "agent_timed_out", "agent_exit_code", "kernel_bootstrap_ok",
    "patch_files", "patch_bytes", "harness_version",
    "model_calls", "cost_usd", "cache_hit_tokens", "cache_miss_tokens", "output_tokens",
    "reasoning_tokens", "mean_first_token_ms", "mean_call_ms", "provider_errors", "retries",
    "parse_repairs", "tool_results", "tool_errors", "truncated_outputs",
    "errors_detected", "failed_commands", "python_errors", "tracebacks", "failing_test_runs",
    "loops_detected", "corrections", "uncorrected_errors",
    "verifications", "verifications_passed", "compactions", "context_tokens_peak",
    "egress_allowed", "egress_refused", "suspicious_commands",
]


def read_jsonl(path):
    rows = []
    for line in path.read_text(errors="replace").splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return rows


def research_records(data_dir):
    research = data_dir / "research"
    records = []
    if research.is_dir():
        for path in sorted(research.glob("*.jsonl")):
            if not path.name.endswith(".acp.jsonl"):
                records.extend(read_jsonl(path))
    return records


def tool_commands(data_dir):
    """Every command or code cell the agent ran, from the session files."""
    commands = []
    for path in (data_dir / "sessions").rglob("*.jsonl") if (data_dir / "sessions").is_dir() else []:
        for record in read_jsonl(path):
            message = record.get("message") or {}
            if message.get("role") != "assistant":
                continue
            for block in message.get("content") or []:
                if block.get("type") == "toolCall":
                    arguments = block.get("arguments") or {}
                    text = arguments.get("command") or arguments.get("code")
                    if isinstance(text, str):
                        commands.append(text)
    return commands


def verifier_counts(task_dir):
    text = (task_dir / "verifier" / "test-output.txt")
    if not text.is_file():
        return None, None
    content = text.read_text(errors="replace")
    required = re.search(r"Required tests: (\d+)", content)
    passed = re.search(r"Required tests that passed: (\d+)", content)
    return (int(required.group(1)) if required else None, int(passed.group(1)) if passed else None)


def summarize_task(task_dir):
    result = json.loads((task_dir / "result.json").read_text()) if (task_dir / "result.json").is_file() else {}
    meta = json.loads((task_dir / "meta.json").read_text()) if (task_dir / "meta.json").is_file() else {}
    data_dir = task_dir / "prime-agent"
    records = research_records(data_dir)
    by_type = {}
    for record in records:
        by_type.setdefault(record.get("event_type"), []).append(record.get("data") or {})

    calls = by_type.get("api.call", [])
    usage = lambda key: sum((call.get("usage") or {}).get(key) or 0 for call in calls)
    first_tokens = [(call.get("latencyMs") or {}).get("firstToken") for call in calls]
    totals = [(call.get("latencyMs") or {}).get("total") for call in calls]
    errors = by_type.get("error.detected", [])
    corrections = by_type.get("correction.delivered", [])
    answered = {correction.get("answers") for correction in corrections}
    checks = by_type.get("verification", [])
    tools = by_type.get("tool.result", [])
    contexts = by_type.get("context.built", [])
    versions = {record.get("harness_version") for record in records if record.get("run_id")}

    egress = read_jsonl(task_dir / "egress.jsonl") if (task_dir / "egress.jsonl").is_file() else []
    suspicious = []
    for command in tool_commands(data_dir):
        for label, pattern in SUSPICIOUS:
            if pattern.search(command):
                suspicious.append({"kind": label, "command": command[:300]})
    required, required_passed = verifier_counts(task_dir)
    stderr = task_dir / "agent" / "stderr.txt"
    skill_failures = (
        stderr.read_text(errors="replace").count("failed to install") if stderr.is_file() else None
    )
    failure = lambda kind: sum(1 for error in errors if error.get("failure") == kind)

    mean = lambda values: round(statistics.mean(v for v in values if v is not None), 1) if any(v is not None for v in values) else None
    row = {
        "instance_id": result.get("instance_id") or meta.get("id") or task_dir.name,
        "repo": meta.get("repo"),
        "language": meta.get("language"),
        "resolved": result.get("resolved", False),
        "resolved_official": result.get("resolved_official", result.get("resolved", False)),
        "patch_differs_from_official": result.get("patch_differs_from_official", False),
        "ignored_files_added": result.get("ignored_files_added", 0),
        "runtime_files_dropped": result.get("runtime_files_dropped", 0),
        "skill_install_failures": skill_failures,
        "required_tests": required,
        "required_tests_passed": required_passed,
        "agent_seconds": result.get("agent_seconds"),
        "agent_timed_out": result.get("agent_timed_out"),
        "agent_exit_code": result.get("agent_exit_code"),
        "kernel_bootstrap_ok": result.get("kernel_bootstrap_ok"),
        "patch_files": result.get("patch_files"),
        "patch_bytes": result.get("patch_bytes"),
        "harness_version": ",".join(sorted(v for v in versions if v)),
        "model_calls": len(calls),
        "cost_usd": round(usage("cost"), 6),
        "cache_hit_tokens": usage("cacheHit"),
        "cache_miss_tokens": usage("cacheMiss"),
        "output_tokens": usage("output"),
        "reasoning_tokens": usage("reasoning"),
        "mean_first_token_ms": mean(first_tokens),
        "mean_call_ms": mean(totals),
        "provider_errors": sum(1 for call in calls if call.get("stopReason") == "error"),
        "retries": sum(1 for retry in by_type.get("api.retry", []) if retry.get("phase") == "start"),
        "parse_repairs": sum(0 if (call.get("parse") or {}).get("clean", True) else 1 for call in calls),
        "tool_results": len(tools),
        "tool_errors": sum(1 for error in errors if error.get("type") == "tool_error"),
        "truncated_outputs": sum(1 for tool in tools if tool.get("truncated")),
        "errors_detected": len(errors),
        "failed_commands": failure("command_exit"),
        "python_errors": failure("python_error"),
        "tracebacks": failure("traceback"),
        "failing_test_runs": failure("test_failure"),
        "loops_detected": sum(1 for error in errors if error.get("type") == "loop"),
        "corrections": len(corrections),
        "uncorrected_errors": sum(1 for error in errors if error.get("id") not in answered),
        "verifications": len(checks),
        "verifications_passed": sum(1 for check in checks if check.get("passed")),
        "compactions": len(by_type.get("context.compacted", [])),
        "context_tokens_peak": max((context.get("estimatedTokens") or 0 for context in contexts), default=0),
        "egress_allowed": sum(1 for entry in egress if entry.get("event") == "allowed"),
        "egress_refused": sum(1 for entry in egress if entry.get("event") == "refused"),
        "suspicious_commands": len(suspicious),
    }
    return row, suspicious


def main():
    results_dir, out_dir = Path(sys.argv[1]), Path(sys.argv[2])
    out_dir.mkdir(parents=True, exist_ok=True)
    task_dirs = sorted(path.parent for path in results_dir.rglob("meta.json"))
    rows = []
    with (out_dir / "tasks.jsonl").open("w") as jsonl:
        for task_dir in task_dirs:
            row, suspicious = summarize_task(task_dir)
            rows.append(row)
            jsonl.write(json.dumps({**row, "suspicious": suspicious}) + "\n")
    with (out_dir / "summary.csv").open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=COLUMNS)
        writer.writeheader()
        writer.writerows(rows)

    resolved = sum(1 for row in rows if row["resolved"])
    official = sum(1 for row in rows if row["resolved_official"])
    cost = sum(row["cost_usd"] for row in rows)
    lines = [
        "# SWE-bench Pro V2 — Prime Agent (Rubiks harness)",
        "",
        f"**Resolved {resolved} of {len(rows)}** ({(100 * resolved / len(rows)) if rows else 0:.1f}%; "
        f"official capture {official} of {len(rows)}) · "
        f"model cost ${cost:.2f} · {sum(row['model_calls'] for row in rows)} model calls · "
        f"{sum(1 for row in rows if row['agent_timed_out'])} timed out · "
        f"{sum(1 for row in rows if row['suspicious_commands'])} tasks with suspicious commands · "
        f"{sum(row['errors_detected'] for row in rows)} errors detected, "
        f"{sum(row['corrections'] for row in rows)} corrections · "
        f"{sum(row['skill_install_failures'] or 0 for row in rows)} skill install failures",
        "",
        "| Repository | Language | Tasks | Resolved | Cost | Mean agent min |",
        "|---|---|---|---|---|---|",
    ]
    for repo in sorted({row["repo"] or "unknown" for row in rows}):
        group = [row for row in rows if (row["repo"] or "unknown") == repo]
        minutes = [row["agent_seconds"] / 60 for row in group if row["agent_seconds"] is not None]
        lines.append(
            f"| {repo} | {group[0]['language']} | {len(group)} | {sum(1 for row in group if row['resolved'])} | "
            f"${sum(row['cost_usd'] for row in group):.2f} | {statistics.mean(minutes):.1f} |"
            if minutes else f"| {repo} | {group[0]['language']} | {len(group)} | - | - | - |"
        )
    lines += ["", "Per-task data: summary.csv / tasks.jsonl; everything the harness wrote: tasks/<instance>/prime-agent/."]
    text = "\n".join(lines) + "\n"
    (out_dir / "summary.md").write_text(text)
    print(text)


if __name__ == "__main__":
    main()
