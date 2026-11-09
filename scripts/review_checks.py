#!/usr/bin/env python3
"""Run review mode's readiness checks for the current branch against its base.

The checks live in .rubiks/checks.toml. On the first run they are detected from the
project's own files and written there for the team to correct; after that the file is
the definition of "ready" and is never re-detected. Built-in checks (scope,
dependencies, secrets) always run. Results go to .rubiks/readiness.md.

Usage: review_checks.py [repo] [--base REF] [--only NAME ...] [--timeout SECONDS]
"""
import argparse
import datetime
import json
import re
import subprocess
import time
from pathlib import Path

from project_map import git

CHECK_HEADER = re.compile(r"^\[\[check\]\]\s*$")
CHECK_FIELD = re.compile(r'^(\w+)\s*=\s*(".*")\s*$')
PATH_TOKEN = re.compile(r"[\w./-]+\.(?:rs|py|ts|tsx|js|jsx|go|toml|json|c|h|cpp|java|kt|swift|rb)\b")
LOCKFILES = {"Cargo.lock", "package-lock.json", "pnpm-lock.yaml", "yarn.lock", "poetry.lock", "uv.lock", "go.sum"}
MANIFESTS = {"Cargo.toml", "package.json", "pyproject.toml", "go.mod", "requirements.txt"}
SECRETS = {
    "private key": re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    "AWS access key": re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    "GitHub token": re.compile(r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
    "API key assignment": re.compile(r"(?i)\b(api[_-]?key|secret|token|password)\b\s*[:=]\s*['\"][^'\"\s]{16,}['\"]"),
}
OUTPUT_TAIL = 40


def detect(repo):
    """Guess checks from files at the repository root, without running anything."""
    checks = []
    found = lambda name: (repo / name).exists()  # noqa: E731
    if found("Cargo.toml"):
        lint = "./script/clippy" if found("script/clippy") else "cargo clippy --workspace --all-targets -- -D warnings"
        checks += [("Format", "cargo fmt --all --check", "Cargo.toml"),
                   ("Lint", lint, "script/clippy" if found("script/clippy") else "Cargo.toml"),
                   ("Tests", "cargo test --workspace", "Cargo.toml")]
    if found("package.json"):
        runner = "pnpm" if found("pnpm-lock.yaml") else "yarn" if found("yarn.lock") else "npm run"
        scripts = json.loads((repo / "package.json").read_text()).get("scripts", {})
        checks += [(name.capitalize(), f"{runner} {name}", "package.json")
                   for name in ("build", "typecheck", "lint", "test") if name in scripts]
    if found("pyproject.toml"):
        settings = (repo / "pyproject.toml").read_text()
        if "[tool.ruff" in settings:
            checks.append(("Lint", "ruff check .", "pyproject.toml"))
        if found("tests") or "[tool.pytest" in settings:
            checks.append(("Tests", "python -m pytest", "pyproject.toml"))
    if found("Makefile") and re.search(r"^check:", (repo / "Makefile").read_text(), re.M):
        checks.append(("Project check", "make check", "Makefile"))
    return checks


def write_checks(path, checks):
    today = datetime.date.today().isoformat()
    lines = ["# Readiness checks for review mode. This file defines what ready means for this",
             "# project: edit, remove or add checks freely. Review mode never re-detects it.",
             f"# The checks below were detected on {today}; confirm or correct them.", ""]
    for name, command, source in checks:
        lines += ["[[check]]", f"name = {json.dumps(name)}", f"command = {json.dumps(command)}",
                  f"source = {json.dumps('detected from ' + source)}", ""]
    path.write_text("\n".join(lines))


def read_checks(path):
    """The subset of TOML this file uses: [[check]] tables of string fields."""
    checks = []
    for number, line in enumerate(path.read_text().splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if CHECK_HEADER.match(line):
            checks.append({})
        elif (field := CHECK_FIELD.match(line)) and checks:
            checks[-1][field.group(1)] = json.loads(field.group(2))
        else:
            raise SystemExit(f"{path}:{number}: expected [[check]] or name = \"text\"")
    for check in checks:
        if not {"name", "command"} <= check.keys():
            raise SystemExit(f"{path}: every [[check]] needs a name and a command")
    return checks


def find_base(repo, requested):
    candidates = [requested] if requested else []
    head = subprocess.run(["git", "-C", str(repo), "symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
                          capture_output=True, text=True)
    candidates += [head.stdout.strip()] if head.returncode == 0 else []
    candidates += ["main", "master"]
    for candidate in candidates:
        merge = subprocess.run(["git", "-C", str(repo), "merge-base", "HEAD", candidate], capture_output=True, text=True)
        if merge.returncode == 0:
            return candidate, merge.stdout.strip()
    raise SystemExit("No base found: pass --base REF.")


def run_check(repo, check, changed, timeout):
    started = time.monotonic()
    try:
        result = subprocess.run(check["command"], shell=True, cwd=repo, capture_output=True,
                                text=True, errors="replace", timeout=timeout)
    except subprocess.TimeoutExpired:
        return "timed out", time.monotonic() - started, f"Stopped after {timeout} s."
    seconds = time.monotonic() - started
    output = (result.stdout + result.stderr).strip()
    if result.returncode == 0:
        return "pass", seconds, output
    if result.returncode == 127:
        return "command not found", seconds, output
    # Separates failures in this change from existing debt. Paths in the output are
    # matched by name, so a failure that names no file counts as this change's.
    named = {token.lstrip("./") for token in PATH_TOKEN.findall(output)}
    outside = named and not any(changed_path.endswith(token) or token.endswith(changed_path)
                                for token in named for changed_path in changed)
    return ("fails only in files this branch didn't change" if outside else "fail"), seconds, output


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("repo", nargs="?", default=".")
    parser.add_argument("--base")
    parser.add_argument("--only", nargs="*", default=[])
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()
    repo = Path(git(args.repo, "rev-parse", "--show-toplevel").strip())
    config = repo / ".rubiks" / "checks.toml"
    config.parent.mkdir(exist_ok=True)
    if not config.exists():
        write_checks(config, detect(repo))
        print(f"Detected checks and wrote {config}. Confirm or correct them.")
    checks = [check for check in read_checks(config) if not args.only or check["name"] in args.only]

    base, merge_base = find_base(repo, args.base)
    untracked = [path for path in git(repo, "ls-files", "--others", "--exclude-standard").splitlines()]
    changed = sorted(set(git(repo, "diff", "--name-only", merge_base).splitlines()) | set(untracked))
    added = deleted = 0
    for line in git(repo, "diff", "--numstat", merge_base).splitlines():
        plus, minus, _ = line.split("\t", 2)
        added += int(plus) if plus.isdigit() else 0
        deleted += int(minus) if minus.isdigit() else 0
    new_lines = [(path, line[1:]) for path, line in _diff_lines(git(repo, "diff", "-U0", merge_base))]
    for path in untracked:
        source = repo / path
        if source.is_file() and source.stat().st_size < 1_000_000:
            text = source.read_text(errors="replace").splitlines()
            new_lines += [(path, line) for line in text]
            added += len(text)
    secrets = [(kind, path) for path, line in new_lines for kind, pattern in SECRETS.items() if pattern.search(line)]
    dependencies = [path for path in changed if Path(path).name in LOCKFILES | MANIFESTS]

    results = []
    for check in checks:
        print(f"Running {check['name']}: {check['command']}", flush=True)
        results.append((check, *run_check(repo, check, changed, args.timeout)))

    short = git(repo, "rev-parse", "--short", merge_base).strip()
    lines = [
        "# Readiness",
        "",
        f"Branch against `{base}` (merge base `{short}`), including uncommitted and untracked files. "
        f"Generated {datetime.datetime.now().isoformat(timespec='minutes')} by `review_checks.py`.",
        "",
        "## Scope",
        "",
        f"{len(changed)} files changed, {added} lines added, {deleted} removed.",
        "",
        "## Checks from `.rubiks/checks.toml`",
        "",
        "| Check | Command | Result | Seconds |",
        "|---|---|---|---|",
        *(f"| {check['name']} | `{check['command']}` | {result} | {seconds:.0f} |"
          for check, result, seconds, _ in results),
        *([] if results else ["", "No checks selected."]),
        "",
        "## Dependencies",
        "",
        *([f"- `{path}` changed" for path in dependencies] or ["No manifest or lockfile changed."]),
        "",
        "## Possible secrets in added lines",
        "",
        *([f"- {kind} in `{path}`: confirm by eye" for kind, path in secrets] or
          ["None matched. This is a pattern scan, so it can miss secrets."]),
        "",
        "## What these checks don't cover",
        "",
        "- Whether the tests exercise the changed lines: there is no coverage data yet.",
        "- Behaviour in a running app or browser.",
        "- Anything the checks file leaves out.",
    ]
    for check, result, _, output in results:
        if result != "pass":
            # Start at the first error when there is one; build progress buries it otherwise.
            output_lines = output.splitlines()
            first_error = next((index for index, line in enumerate(output_lines)
                                if re.match(r"\s*(error|FAILED|failed)\b", line)), None)
            shown = output_lines[first_error:] if first_error is not None else output_lines[-OUTPUT_TAIL:]
            tail = "\n".join(shown[:OUTPUT_TAIL])
            lines += ["", f"## Output: {check['name']} ({result})", "", "```", tail, "```"]
    report = repo / ".rubiks" / "readiness.md"
    report.write_text("\n".join(lines) + "\n")
    for check, result, seconds, _ in results:
        print(f"  {check['name']}: {result} ({seconds:.0f} s)")
    print(f"{len(changed)} files changed; {len(dependencies)} dependency files; "
          f"{len(secrets)} possible secrets. Report: {report}")


def _diff_lines(diff):
    """Added lines of a unified diff, with the file each belongs to."""
    current = ""
    for line in diff.splitlines():
        if line.startswith("+++ "):
            current = line[6:] if line.startswith("+++ b/") else ""
        elif line.startswith("+") and current:
            yield current, line


if __name__ == "__main__":
    main()
