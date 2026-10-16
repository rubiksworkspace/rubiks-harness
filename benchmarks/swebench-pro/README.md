# SWE-bench Pro on the Rubiks harness

`.github/workflows/swebench-pro.yml` runs Prime Agent (this repository's
harness) on [SWE-bench Pro V2](https://github.com/scaleapi/SWE-bench_Pro-os)
in GitHub Actions and keeps everything the harness produces.

## Running it

- Push to the `benchmark` branch: the 100 tasks of `harness100.csv` on
  DeepSeek V4 Flash.
- Or start it by hand (once the workflow is on the default branch, or with
  `gh workflow run swebench-pro.yml --ref benchmark -f count=50`) to choose
  the task count, exact instance ids, the task pool (`harness100`, `all`,
  `hard51`), the model, or the selection seed.

The repository secret `DEEPSEEK_API_KEY` must be set.

## The harness100 task pool

On a random 100 tasks the model resolved 97: the task statements spell out
requirements and interfaces, so those tasks mostly measure whether the model
can implement a spec. `pick_harness_tasks.py` picks tasks whose reference fix
is large in the ways the harness has to carry, ranking each stressor against
all 642 tasks (top quarter counts):

| Stressor | Exercises in the harness |
|---|---|
| files changed | multi-file navigation and search |
| directories changed | cross-module changes, context management |
| lines changed | long edits, edit-tool robustness, long horizon |
| new files | creating files, project structure, patch capture |
| failing tests to fix | test loops, error detection and correction |
| HARD-51 | the benchmark's own hard subset |

Every stressor gets its best tasks first, then the list fills by overall
score, at most 12 tasks per repository. Tasks an earlier run already resolved
are left out, since they no longer separate one harness from another. To
rebuild it (from a checkout of SWE-bench_Pro-os and the dataset parquet):

    uv run --with pyarrow benchmarks/swebench-pro/pick_harness_tasks.py \
      --dataset test.parquet --tasks-dir sbp/v2/tasks --hard51 sbp/v2/hard51_ids.txt \
      --exclude <earlier run>/tasks.jsonl

## What one task does (`run_task.sh`)

The SWE-bench Pro V2 locked protocol:

1. **Setup** (network on): pull `ghcr.io/scaleapi/swe-bench_pro-v2:<id>`, mount
   the static harness, prepare its Python kernel.
2. **Agent** (network off): the container moves to an internal network whose
   only exit is `egress_proxy.py`, which reaches `api.deepseek.com` and
   refuses everything else (every attempt is logged). Prime Agent runs
   headless (`prime-agent -p`) on the task's `instruction.md`, 50 minutes at
   most. The diff is captured twice: the official way (`git add -A; git diff
   --cached`), and corrected: new files the agent wrote that `.gitignore`
   hides are added, and runtime junk (`*.aof`, `*.pyc`, logs, caches) is
   dropped.
3. **Verify**: a fresh container from the same image applies the corrected
   diff and runs the task's own `tests/test.sh`; reward 1 means resolved.
   When the official diff differs, it is graded too, so both scores are
   reported (`resolved` and `resolved_official`).

## What is kept

Per task (artifact `task-<instance id>`):

| Path | Contents |
|---|---|
| `result.json` | verdict, timings, exit codes, patch size |
| `meta.json` | instance id, repository, language |
| `model.patch` | the agent's diff (corrected capture, the one graded as `resolved`) |
| `model.official.patch` | the diff captured the official way |
| `prime-agent/` | the harness's whole data dir: `research/` (research log + blobs, with content consent), `sessions/`, `session-artifacts/`, `harness/` (memory and refinements), `logs/` |
| `agent/` | the agent's stdout and stderr, kernel bootstrap, image pull and capture logs (`capture.log`: files added or dropped by the corrected capture) |
| `egress.jsonl` | every connection the agent's sandbox attempted |
| `verifier/` | test output, `output.json`, `reward.txt`, patch apply log |
| `verifier-official/` | the same for `model.official.patch`, only when the two diffs differ |

The run's `swebench-pro-results` artifact merges every task with
`summary.csv` (one row per task: verdict next to the research-log metrics and
the anti-cheating signals), `tasks.jsonl` and `summary.md`.

The repository is public, so these artifacts (including prompts, reasoning
and agent commands) can be downloaded by any signed-in GitHub user.
