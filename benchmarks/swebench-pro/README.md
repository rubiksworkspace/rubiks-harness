# SWE-bench Pro on the Rubiks harness

`.github/workflows/swebench-pro.yml` runs Prime Agent (this repository's
harness) on [SWE-bench Pro V2](https://github.com/scaleapi/SWE-bench_Pro-os)
in GitHub Actions and keeps everything the harness produces.

## Running it

- Push to the `benchmark` branch: 20 tasks, spread over the 11 repositories,
  on DeepSeek V4 Flash.
- Or start it by hand (once the workflow is on the default branch, or with
  `gh workflow run swebench-pro.yml --ref benchmark -f count=50`) to choose
  the task count, exact instance ids, the HARD-51 subset, the model, or the
  selection seed.

The repository secret `DEEPSEEK_API_KEY` must be set.

## What one task does (`run_task.sh`)

The SWE-bench Pro V2 locked protocol:

1. **Setup** (network on): pull `ghcr.io/scaleapi/swe-bench_pro-v2:<id>`, mount
   the static harness, prepare its Python kernel.
2. **Agent** (network off): the container moves to an internal network whose
   only exit is `egress_proxy.py`, which reaches `api.deepseek.com` and
   refuses everything else (every attempt is logged). Prime Agent runs
   headless (`prime-agent -p`) on the task's `instruction.md`, 50 minutes at
   most. The answer is the repository's `git diff`, captured the official way.
3. **Verify**: a fresh container from the same image applies the diff and runs
   the task's own `tests/test.sh`; reward 1 means resolved.

## What is kept

Per task (artifact `task-<instance id>`):

| Path | Contents |
|---|---|
| `result.json` | verdict, timings, exit codes, patch size |
| `meta.json` | instance id, repository, language |
| `model.patch` | the agent's diff |
| `prime-agent/` | the harness's whole data dir: `research/` (research log + blobs, with content consent), `sessions/`, `session-artifacts/`, `harness/` (memory and refinements), `logs/` |
| `agent/` | the agent's stdout and stderr, kernel bootstrap and image pull logs |
| `egress.jsonl` | every connection the agent's sandbox attempted |
| `verifier/` | test output, `output.json`, `reward.txt`, patch apply log |

The run's `swebench-pro-results` artifact merges every task with
`summary.csv` (one row per task: verdict next to the research-log metrics and
the anti-cheating signals), `tasks.jsonl` and `summary.md`.

The repository is public, so these artifacts (including prompts, reasoning
and agent commands) can be downloaded by any signed-in GitHub user.
