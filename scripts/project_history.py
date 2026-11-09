#!/usr/bin/env python3
"""Generate .rubiks/architecture.md: how the project's shape changed, from git alone.

Facts only, with no model-written text: areas added and removed, dependency changes,
public symbols added and removed, hotspots, and commits that touch decision records.
Areas and symbols use the same rules as project_map.py.

Usage: project_history.py [repo] [--since "6 months ago"]
"""
import argparse
import collections
import re
import subprocess
from pathlib import Path

from project_map import LANGUAGES, SKIPPED_DIRS, area_of, cell, git, symbols

MANIFESTS = ("Cargo.toml", "package.json")
CARGO_SECTION = re.compile(r"^\[(?:workspace\.|target\.[^\]]+\.)?(?:dev-|build-)?dependencies(?:\.([A-Za-z0-9_-]+))?\]")
CARGO_ENTRY = re.compile(r"^([A-Za-z0-9_-]+)\s*=")
JSON_SECTION = re.compile(r'^\s*"(?:dev|peer|optional)?[dD]ependencies"\s*:\s*\{')
JSON_ENTRY = re.compile(r'^\s*"([^"]+)"\s*:')
DECISION_PATH = re.compile(r"(^|/)(adr|decisions)/")
MAX_ROWS = 40
MAX_NAMES = 8


def vendored(path):
    return bool(SKIPPED_DIRS.intersection(path.split("/")[:-1]))


def renamed(path):
    """numstat writes renames as 'a => b' or 'x/{a => b}/y'; keep the new path."""
    path = re.sub(r"\{[^{}]* => ([^{}]*)\}", r"\1", path)
    return path.split(" => ")[-1].replace("//", "/")


def commits(repo, since):
    """(hash, date, subject, [(added, deleted, path)]) oldest first."""
    found = []
    for line in git(repo, "log", f"--since={since}", "--reverse", "--numstat",
                    "--format=@%h\t%ad\t%s", "--date=short").splitlines():
        if line.startswith("@"):
            sha, date, subject = (line[1:].split("\t", 2) + ["", ""])[:3]
            found.append((sha, date, subject, []))
        elif line and found:
            added, deleted, path = line.split("\t", 2)
            lines = (int(added) if added.isdigit() else 0) + (int(deleted) if deleted.isdigit() else 0)
            found[-1][3].append((lines, renamed(path)))
    return found


def areas_at(repo, rev):
    return {area_of(path) for path in git(repo, "ls-tree", "-r", "--name-only", rev).splitlines()}


def dependency_changes(repo, since):
    """Walk full-context manifest diffs, so each changed line's section is known."""
    rows, sha, date, manifest, section = [], "", "", "", False
    pending = collections.defaultdict(set)

    def flush():
        for (path, name), signs in sorted(pending.items()):
            if vendored(path):
                continue
            change = "changed" if signs == {"+", "-"} else "added" if signs == {"+"} else "removed"
            rows.append((sha, date, path, name, change))
        pending.clear()

    log = git(repo, "log", f"--since={since}", "--reverse", "-p", "-U99999", "--format=@%h\t%ad",
              "--date=short", "--", *[f"*{name}" for name in MANIFESTS])
    for line in log.splitlines():
        if line.startswith("@") and "\t" in line:
            flush()
            sha, date = line[1:].split("\t", 1)
        elif line.startswith("+++ "):
            manifest, section = line[6:] if line.startswith("+++ b/") else "", False
        elif line[:1] in (" ", "+", "-") and manifest and not line.startswith(("--- ", "+++ ")):
            body = line[1:]
            if manifest.endswith(".toml"):
                header = CARGO_SECTION.match(body)
                if body.startswith("["):
                    section = bool(header)
                    if header and header.group(1) and line[0] in "+-":
                        pending[(manifest, header.group(1))].add(line[0])
                    continue
                entry = CARGO_ENTRY.match(body) if section else None
            else:
                if JSON_SECTION.match(body):
                    section = True
                    continue
                if body.strip().startswith("}"):
                    section = False
                entry = JSON_ENTRY.match(body) if section else None
            if entry and line[0] in "+-":
                pending[(manifest, entry.group(1))].add(line[0])
    flush()
    return rows


def surface_changes(repo, baseline, paths):
    """Public symbols added and removed per area between baseline and HEAD."""
    wanted = [path for path in paths if not vendored(path) and LANGUAGES.get(Path(path).suffix.lower(), "Markdown") != "Markdown"]
    requests = "".join(f"{rev}:{path}\n" for path in wanted for rev in (baseline, "HEAD"))
    batch = subprocess.run(["git", "-C", str(repo), "cat-file", "--batch"], input=requests.encode(),
                           capture_output=True, check=True).stdout
    texts, offset = [], 0
    while offset < len(batch):
        end = batch.index(b"\n", offset)
        header = batch[offset:end].split()
        offset = end + 1
        if len(header) == 2 and header[1] == b"missing":
            texts.append("")
            continue
        size = int(header[2])
        texts.append(batch[offset:offset + size].decode(errors="replace"))
        offset += size + 1
    changes = collections.defaultdict(lambda: (set(), set()))
    for index, path in enumerate(wanted):
        language = LANGUAGES[Path(path).suffix.lower()]
        old, new = set(symbols(language, texts[2 * index])), set(symbols(language, texts[2 * index + 1]))
        added, removed = changes[area_of(path)]
        added.update(new - old)
        removed.update(old - new)
    return {area: change for area, change in changes.items() if change[0] or change[1]}


def names(values):
    values = sorted(values)
    return ", ".join(values[:MAX_NAMES]) + (f" +{len(values) - MAX_NAMES}" if len(values) > MAX_NAMES else "")


def table(header, rows, total=None):
    lines = [f"| {' | '.join(header)} |", f"|{'---|' * len(header)}"]
    lines += [f"| {' | '.join(cell(str(value)) for value in row)} |" for row in rows[:MAX_ROWS]]
    if (total or len(rows)) > MAX_ROWS:
        lines.append(f"\n{(total or len(rows)) - MAX_ROWS} more not shown.")
    return lines if rows else ["None in this period."]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("repo", nargs="?", default=".")
    parser.add_argument("--since", default="6 months ago")
    args = parser.parse_args()
    repo = Path(git(args.repo, "rev-parse", "--show-toplevel").strip())
    head = git(repo, "rev-parse", "--short", "HEAD").strip()
    history = commits(repo, args.since)
    if not history:
        raise SystemExit(f"No commits since {args.since}.")
    first = history[0][0]
    has_parent = subprocess.run(["git", "-C", str(repo), "rev-parse", "-q", "--verify", f"{first}^"],
                                capture_output=True).returncode == 0
    baseline = f"{first}^" if has_parent else first

    touched = collections.defaultdict(list)
    churn = collections.Counter()
    decisions = []
    for sha, date, subject, files in history:
        areas = {area_of(path) for _, path in files}
        for area in areas:
            touched[area].append((sha, date))
        for lines, path in files:
            churn[area_of(path)] += lines
        if any(DECISION_PATH.search(path) for _, path in files):
            others = sorted(area for area in areas if not any(
                DECISION_PATH.search(path) and area_of(path) == area for _, path in files))
            decisions.append((f"{sha} {date}", subject[:80], names(others)))

    before, now = areas_at(repo, baseline), areas_at(repo, "HEAD")
    area_rows = [("added", area, " ".join(touched[area][0]) if touched[area] else "")
                 for area in sorted(now - before)]
    area_rows += [("removed", area, " ".join(touched[area][-1]) if touched[area] else "")
                  for area in sorted(before - now)]
    hotspots = sorted(touched, key=lambda area: (-len(touched[area]), -churn[area]))
    changed_paths = git(repo, "diff", "--name-only", baseline, "HEAD").splitlines()
    surface = surface_changes(repo, baseline, changed_paths)
    dependencies = dependency_changes(repo, args.since)

    lines = [
        "# Architecture history",
        "",
        f"Generated by `project_history.py` from git only. Describes commit `{head}`, "
        f"over {len(history)} commits since {history[0][1]} ({args.since}). Don't edit by hand.",
        "",
        "Symbols and dependencies are found by pattern matching, not by compiling, so treat "
        "them as a guide.",
        "",
        "## Areas added or removed",
        "",
        *table(("Change", "Area", "Commit"), area_rows),
        "",
        "## Dependency changes",
        "",
        *table(("Commit", "Manifest", "Dependency", "Change"),
               [(f"{sha} {date}", path, name, change) for sha, date, path, name, change in reversed(dependencies)]),
        "",
        "## Public symbols added or removed",
        "",
        *table(("Area", "Added", "Removed"),
               [(area, names(added), names(removed)) for area, (added, removed) in
                sorted(surface.items(), key=lambda item: -(len(item[1][0]) + len(item[1][1])))]),
        "",
        "## Hotspots",
        "",
        *table(("Area", "Commits", "Lines changed", "Last change"),
               [(area, len(touched[area]), churn[area], " ".join(touched[area][-1])) for area in hotspots]),
        "",
        "## Decision records",
        "",
        *table(("Commit", "Subject", "Other areas changed"), list(reversed(decisions))),
    ]
    target = repo / ".rubiks" / "architecture.md"
    target.parent.mkdir(exist_ok=True)
    target.write_text("\n".join(lines) + "\n")
    print(f"{target}: {len(history)} commits, {len(dependencies)} dependency changes, "
          f"{len(surface)} areas with symbol changes")


if __name__ == "__main__":
    main()
