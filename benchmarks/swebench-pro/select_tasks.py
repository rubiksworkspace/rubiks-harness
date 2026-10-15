#!/usr/bin/env python3
"""Pick the SWE-bench Pro V2 tasks one benchmark run covers.

Writes the GitHub Actions matrix as JSON: [{"id", "repo", "language"}, ...].
Explicit `--instances` win; otherwise `--count` tasks are spread round-robin
over the repositories (so every repository is represented), chosen within
each repository by a seeded shuffle, so the same seed picks the same tasks.

Usage:
  select_tasks.py --tasks-dir v2/tasks --dataset test.parquet \
      [--count 40] [--seed 1] [--subset all|hard51] [--instances id1,id2]
"""
import argparse
import json
import random
import sys
from pathlib import Path

import pyarrow.parquet as pq

# GitHub Actions caps a matrix at 256 jobs.
MAX_TASKS = 256


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tasks-dir", type=Path, required=True)
    parser.add_argument("--dataset", type=Path, required=True, help="the HuggingFace test split (parquet)")
    parser.add_argument("--hard51", type=Path, help="v2/hard51_ids.txt, for --subset hard51")
    parser.add_argument("--count", type=int, default=40)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--subset", choices=["all", "hard51"], default="all")
    parser.add_argument("--instances", default="", help="comma-separated instance ids (overrides --count)")
    args = parser.parse_args()

    rows = {row["instance_id"]: row for row in pq.read_table(args.dataset).to_pylist()}
    available = sorted(path.name for path in args.tasks_dir.iterdir() if (path / "task.toml").is_file())
    if args.subset == "hard51":
        if args.hard51 is None:
            sys.exit("--subset hard51 needs --hard51")
        wanted = {line.strip() for line in args.hard51.read_text().splitlines() if line.strip()}
        available = [task for task in available if task in wanted]

    if args.instances.strip():
        chosen = [task.strip() for task in args.instances.split(",") if task.strip()]
        unknown = [task for task in chosen if task not in available]
        if unknown:
            sys.exit(f"unknown instance ids: {', '.join(unknown)}")
    else:
        by_repo = {}
        for task in available:
            repo = rows.get(task, {}).get("repo", "unknown")
            by_repo.setdefault(repo, []).append(task)
        rng = random.Random(args.seed)
        for tasks in by_repo.values():
            rng.shuffle(tasks)
        chosen = []
        count = min(args.count, len(available), MAX_TASKS)
        while len(chosen) < count:
            for repo in sorted(by_repo):
                if by_repo[repo] and len(chosen) < count:
                    chosen.append(by_repo[repo].pop())

    if len(chosen) > MAX_TASKS:
        sys.exit(f"at most {MAX_TASKS} tasks per run")
    matrix = [
        {
            "id": task,
            "repo": rows.get(task, {}).get("repo", "unknown"),
            "language": rows.get(task, {}).get("repo_language", "unknown"),
        }
        for task in chosen
    ]
    json.dump(matrix, sys.stdout)
    print(file=sys.stderr)
    for entry in matrix:
        print(f"{entry['language']:<7} {entry['repo']:<28} {entry['id']}", file=sys.stderr)


if __name__ == "__main__":
    main()
