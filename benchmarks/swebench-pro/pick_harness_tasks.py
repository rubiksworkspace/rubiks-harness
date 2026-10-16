#!/usr/bin/env python3
"""Pick the SWE-bench Pro V2 tasks that stress the harness, not the model.

On the default 100-task run the model resolved 97: the tasks' requirements
and interface sections spell out most of the fix, so they measure whether
the model can implement a spec. This picks tasks whose reference fix is
large in ways the harness has to carry: many files and directories to find
and keep in context, long edits, new files, many failing tests to run and
fix, plus the benchmark's own HARD-51 tasks. Each task is ranked on those
stressors (percentile within all 642 tasks), and the pick is spread over the
repositories and over the stressors, so every harness capability is covered.

Writes harness100.csv (id, repository, language, the stressor values, the
stressors the task was picked for). select_tasks.py --subset harness100
reads it.

Usage:
  pick_harness_tasks.py --dataset test.parquet --tasks-dir v2/tasks \
      --hard51 v2/hard51_ids.txt [--exclude tasks.jsonl ...] [--count 100] \
      [--out harness100.csv]

--exclude takes a summarize.py tasks.jsonl: tasks that run resolved are
left out, since they no longer separate one harness from another.
"""
import argparse
import bisect
import csv
import json
import re
from pathlib import Path

import pyarrow.parquet as pq

# What each stressor exercises in the harness.
STRESSORS = {
    "files": "multi-file navigation and search",
    "dirs": "cross-module changes, context management",
    "lines": "long edits, edit-tool robustness, long horizon",
    "new_files": "creating files, project structure, patch capture",
    "fail_to_pass": "test loops, error detection and correction",
}
# A task counts as stressing a capability from this percentile up.
STRESSED = 0.75
# At most this many tasks per repository (tutanota has only 4).
PER_REPO = 12


def features(row):
    patch = row["patch"]
    files = re.findall(r"^diff --git a/(\S+)", patch, re.M)
    return {
        "files": len(files),
        "dirs": len({path.rsplit("/", 1)[0] if "/" in path else "." for path in files}),
        "lines": sum(
            1 for line in patch.splitlines()
            if line[:1] in "+-" and not line.startswith(("+++", "---"))
        ),
        "new_files": len(re.findall(r"^new file mode", patch, re.M)),
        "fail_to_pass": len(json.loads(row["fail_to_pass"])),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset", type=Path, required=True)
    parser.add_argument("--tasks-dir", type=Path, required=True)
    parser.add_argument("--hard51", type=Path, required=True)
    parser.add_argument("--exclude", type=Path, action="append", default=[])
    parser.add_argument("--count", type=int, default=100)
    parser.add_argument("--out", type=Path, default=Path(__file__).with_name("harness100.csv"))
    args = parser.parse_args()

    available = {path.name for path in args.tasks_dir.iterdir() if (path / "task.toml").is_file()}
    hard51 = set(args.hard51.read_text().split())
    resolved = set()
    for path in args.exclude:
        for line in path.read_text().splitlines():
            row = json.loads(line)
            if row.get("resolved"):
                resolved.add(row["instance_id"])

    rows = [row for row in pq.read_table(args.dataset).to_pylist() if row["instance_id"] in available]
    values = {row["instance_id"]: features(row) for row in rows}
    # Percentile of each value within all tasks (ties share the lower rank).
    sorted_values = {key: sorted(value[key] for value in values.values()) for key in STRESSORS}
    percentile = lambda key, value: bisect.bisect_left(sorted_values[key], value) / len(sorted_values[key])

    candidates = []
    for row in rows:
        task = row["instance_id"]
        if task in resolved:
            continue
        ranks = {key: percentile(key, values[task][key]) for key in STRESSORS}
        stressed = [key for key in STRESSORS if ranks[key] >= STRESSED and values[task][key] > 0]
        hard = task in hard51
        if hard:
            stressed.append("hard51")
        score = sum(ranks.values()) / len(ranks) + 0.25 * len(stressed) + (0.5 if hard else 0)
        candidates.append({
            "instance_id": task,
            "repo": row["repo"],
            "language": row["repo_language"],
            **values[task],
            "stressors": stressed,
            "score": round(score, 3),
        })
    candidates.sort(key=lambda task: (-task["score"], task["instance_id"]))

    # Cover every stressor first (its best tasks, round-robin), then fill by
    # score; the repository cap holds throughout.
    chosen, per_repo = [], {}
    def take(task):
        if task in chosen or per_repo.get(task["repo"], 0) >= PER_REPO or len(chosen) >= args.count:
            return
        chosen.append(task)
        per_repo[task["repo"]] = per_repo.get(task["repo"], 0) + 1

    tracks = [*STRESSORS, "hard51"]
    queues = {track: [task for task in candidates if track in task["stressors"]] for track in tracks}
    floor = args.count // (2 * len(tracks))
    for _ in range(floor):
        for track in tracks:
            while queues[track]:
                task = queues[track].pop(0)
                if task not in chosen and per_repo.get(task["repo"], 0) < PER_REPO:
                    take(task)
                    break
    for task in candidates:
        take(task)

    chosen.sort(key=lambda task: (task["repo"], -task["score"]))
    with args.out.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=[
            "instance_id", "repo", "language", *STRESSORS, "stressors", "score",
        ])
        writer.writeheader()
        for task in chosen:
            writer.writerow({**task, "stressors": " ".join(task["stressors"])})

    print(f"{len(chosen)} tasks from {len(candidates)} candidates ({len(resolved)} excluded as resolved)")
    for track in tracks:
        print(f"  {track:<13} {sum(1 for task in chosen if track in task['stressors']):>3}  {STRESSORS.get(track, 'the benchmark hard subset')}")
    for repo in sorted(per_repo):
        print(f"  {repo:<30} {per_repo[repo]}")


if __name__ == "__main__":
    main()
