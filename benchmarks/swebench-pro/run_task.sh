#!/usr/bin/env bash
# Run Prime Agent on one SWE-bench Pro V2 task under the locked protocol, keep
# everything the harness wrote, and grade the patch on a fresh container.
#
#   run_task.sh <task dir> <harness dir> <uv binary> <output dir>
#
# Environment: PROVIDER (openrouter, the default, or deepseek) and its key
# (OPENROUTER_API_KEY or DEEPSEEK_API_KEY), MODEL (default deepseek-v4-flash;
# through OpenRouter it runs as deepseek/<model>), THINKING (default high), AGENT_TIMEOUT_SEC (default 3000, the protocol's
# 50 minutes), VERIFY_TIMEOUT_SEC (default 3000).
#
# Phases (SWE-bench Pro V2 locked protocol, v2/README.md):
#   setup   network on: pull the image, prepare the harness's Python kernel
#   agent   network off except the model API (an allow-list proxy); the agent
#           works in /app (or /testbed); its `git diff` is the answer
#   verify  a fresh container from the same image: apply the diff, run the
#           task's own tests/test.sh; reward 1 = resolved
#
# Output (all of it is uploaded):
#   result.json            the run's facts and verdicts
#   model.patch            the agent's diff, corrected: also new files the agent
#                          wrote under git-ignored paths, minus runtime junk its
#                          own runs left behind (agent/capture.log lists both)
#   model.official.patch   the official capture (`git add -A`), graded
#                          separately into verifier-official/ when it differs
#   agent/                 the agent's stdout/stderr, kernel bootstrap log
#   prime-agent/           the harness's whole data dir: research log
#                          (+ blobs), sessions, session artifacts, harness
#                          memory, logs (not the kernel venv)
#   egress.jsonl           every connection the agent's sandbox attempted
#   verifier/              test.sh output, output.json, reward.txt, apply log
set -uo pipefail

task_dir="$(cd "$1" && pwd)"
harness_dir="$(cd "$2" && pwd)"
uv_bin="$(cd "$(dirname "$3")" && pwd)/$(basename "$3")"
out="$(mkdir -p "$4" && cd "$4" && pwd)"
here="$(cd "$(dirname "$0")" && pwd)"

provider="${PROVIDER:-openrouter}"
model="${MODEL:-deepseek-v4-flash}"
case "$provider" in
  openrouter) api_host=openrouter.ai; key_var=OPENROUTER_API_KEY; model="deepseek/${model#deepseek/}" ;;
  deepseek) api_host=api.deepseek.com; key_var=DEEPSEEK_API_KEY ;;
  *) echo "unknown PROVIDER: $provider (openrouter or deepseek)" >&2; exit 2 ;;
esac
if [ -z "${!key_var:-}" ]; then
  echo "$key_var is not set" >&2
  exit 2
fi
thinking="${THINKING:-high}"
agent_timeout="${AGENT_TIMEOUT_SEC:-3000}"
verify_timeout="${VERIFY_TIMEOUT_SEC:-3000}"
instance_id="$(basename "$task_dir")"
image="$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["environment"]["docker_image"])' "$task_dir/task.toml")"

suffix="${instance_id: -12}-$$"
agent="sbp-agent-$suffix"
egress="sbp-egress-$suffix"
net="sbp-net-$suffix"
mkdir -p "$out/agent" "$out/prime-agent" "$out/verifier"

now() { date +%s; }
log() { echo "[$(date -u +%H:%M:%S)] $*"; }

cleanup() {
  docker logs "$egress" > "$out/egress.jsonl" 2>/dev/null || true
  docker rm -f "$agent" "$egress" > /dev/null 2>&1 || true
  docker network rm "$net" > /dev/null 2>&1 || true
}
trap cleanup EXIT

# --- setup ------------------------------------------------------------------
log "pulling $image"
setup_start=$(now)
pull_ok=true
docker pull --platform linux/amd64 "$image" > "$out/agent/pull.log" 2>&1 || pull_ok=false

docker network create --internal "$net" > /dev/null
docker run -d --name "$egress" --network bridge \
  -v "$here/egress_proxy.py:/egress_proxy.py:ro" \
  python:3.12-alpine python /egress_proxy.py "$api_host" > /dev/null
docker network connect --alias egress "$net" "$egress"

docker run -d --name "$agent" --platform linux/amd64 --network bridge \
  -v "$harness_dir:/opt/prime-agent:ro" \
  -v "$uv_bin:/usr/local/bin/uv:ro" \
  -e HOME=/root \
  --entrypoint sleep "$image" infinity > /dev/null

repo_dir="$(docker exec "$agent" sh -c 'cd /app 2>/dev/null || cd /testbed; pwd')"
log "repository at $repo_dir"

# The kernel venv and the skills' Python packages are installed now, while
# the sandbox still has network (from the repository, so the same skills load
# as in the session).
bootstrap_ok=true
docker exec -w "$repo_dir" "$agent" /opt/prime-agent/prime-agent --prime-agent-bootstrap \
  > "$out/agent/kernel-bootstrap.log" 2>&1 || bootstrap_ok=false
setup_seconds=$(( $(now) - setup_start ))

# --- agent ------------------------------------------------------------------
docker network disconnect bridge "$agent"
docker network connect "$net" "$agent"

# The budget is enforced inside the sandbox (the patch so far is still
# graded); an image without `timeout` gets it from outside instead.
if docker exec "$agent" sh -c 'command -v timeout' > /dev/null 2>&1; then
  inner_timeout=(timeout -k 30 "$agent_timeout")
  outer_timeout=()
else
  inner_timeout=()
  outer_timeout=(timeout -k 30 "$agent_timeout")
fi

# Files changed after this are the agent's (the patch capture reads it).
docker exec "$agent" touch /tmp/.sbp-agent-start

log "running Prime Agent ($model, thinking $thinking, ${agent_timeout}s budget)"
agent_start=$(now)
"${outer_timeout[@]}" docker exec -w "$repo_dir" \
  -e "$key_var" \
  -e HTTPS_PROXY=http://egress:8888 -e https_proxy=http://egress:8888 \
  -e HTTP_PROXY=http://egress:8888 -e http_proxy=http://egress:8888 \
  -e NO_PROXY=localhost,127.0.0.1 -e no_proxy=localhost,127.0.0.1 \
  -e PI_SKIP_VERSION_CHECK=1 \
  -e PRIME_AGENT_RESEARCH=1 \
  -e PRIME_AGENT_RESEARCH_CONSENT=content \
  -e PRIME_AGENT_PARTICIPANT=swebench-pro-ci \
  -e PRIME_AGENT_TASK="$instance_id" \
  "$agent" "${inner_timeout[@]}" \
  /opt/prime-agent/prime-agent -p --offline \
    --provider "$provider" --model "$model" --thinking "$thinking" \
    "$(cat "$task_dir/instruction.md")" \
  > "$out/agent/stdout.txt" 2> "$out/agent/stderr.txt"
agent_exit=$?
agent_seconds=$(( $(now) - agent_start ))
# An outside timeout leaves the agent running in the sandbox: stop it before
# reading the diff.
docker exec "$agent" sh -c 'pkill -f /opt/prime-agent/prime-agent' > /dev/null 2>&1 || true
timed_out=false
[ "$agent_exit" -eq 124 ] || [ "$agent_exit" -eq 137 ] && timed_out=true
log "agent finished: exit $agent_exit after ${agent_seconds}s"

# The official capture (v2/tooling/locked_mini_swe.py): every change, staged.
docker exec -w "$repo_dir" "$agent" sh -c \
  'git add -A 2>/dev/null; git diff --cached; git reset -q 2>/dev/null' > "$out/model.official.patch" 2> "$out/agent/diff-stderr.txt"

# The corrected capture. `git add -A` skips new files under git-ignored paths
# (a repository can ignore real source), skips files git hides some other
# way (sparse checkout, skip-worktree or assume-unchanged index flags: both
# ansible tasks of the 100-task run came out empty although the agent wrote
# the module), and keeps whatever the agent's own test runs left behind
# (database dumps, caches, logs). So every file the agent wrote that is still
# not staged after `git add -A` is added by force, whatever hid it.
capture='
marker=/tmp/.sbp-agent-start
echo "git: core.sparseCheckout=$(git config core.sparseCheckout) hidden index entries=$(git ls-files -v | grep -c "^[a-zS]")" >&2
git add -A 2>/dev/null
find . \( -name .git -o -name node_modules -o -name __pycache__ -o -name .pytest_cache \
  -o -name .mypy_cache -o -name .hypothesis -o -name .tox -o -name .venv -o -name venv \
  -o -name .cache -o -name coverage -o -name .next -o -name dist -o -name build \) -prune \
  -o -type f -newer "$marker" -print 2>/dev/null |
while IFS= read -r file; do
  file=${file#./}
  flag=$(git ls-files -v -- "$file" 2>/dev/null | cut -c1)
  case "$flag" in
    H|"") ;;
    *)
      echo "unhid index entry ($flag): $file" >&2
      git update-index --no-skip-worktree --no-assume-unchanged -- "$file"
      git add -f --sparse -- "$file" 2>/dev/null || git add -f -- "$file"
      continue ;;
  esac
  [ -z "$(git diff --cached --name-only -- "$file")" ] || continue
  [ -n "$flag" ] && continue
  if git check-ignore -q -- "$file" 2>/dev/null; then
    echo "added ignored file: $file" >&2
  else
    echo "added hidden file: $file" >&2
  fi
  git add -f --sparse -- "$file" 2>/dev/null || git add -f -- "$file"
done
git diff --cached --name-only --diff-filter=A | while IFS= read -r file; do
  case "$file" in
    *.aof|*.rdb|*.pyc|*.log|*.orig|*.rej|*.swp|.coverage|coverage.xml|nohup.out|\
    .hypothesis/*|*/.hypothesis/*|\
    __pycache__/*|*/__pycache__/*|.pytest_cache/*|*/.pytest_cache/*|node_modules/*|*/node_modules/*)
      echo "dropped runtime file: $file" >&2
      git reset -q -- "$file" ;;
  esac
done
git diff --cached
git reset -q 2>/dev/null'
docker exec -w "$repo_dir" "$agent" sh -c "$capture" > "$out/model.patch" 2> "$out/agent/capture.log"

# Everything the harness wrote, without the rebuildable kernel venv.
docker exec "$agent" sh -c \
  'cd /root/.prime/agent 2>/dev/null && tar --exclude=./kernel-venv --exclude="*.sock" -cf - .' \
  2> "$out/agent/collect-stderr.txt" | tar -xf - -C "$out/prime-agent" 2>> "$out/agent/collect-stderr.txt" || true
if [ -z "$(ls -A "$out/prime-agent")" ]; then
  # No tar in the image: copy the whole directory, then drop the venv.
  docker cp "$agent:/root/.prime/agent/." "$out/prime-agent/" >> "$out/agent/collect-stderr.txt" 2>&1 || true
  rm -rf "$out/prime-agent/kernel-venv"
fi

cleanup
trap - EXIT

# --- verify -----------------------------------------------------------------
# Grade one patch on a fresh container with the task's own tests/test.sh.
grade() {
  local patch="$1" dir="$2"
  mkdir -p "$dir"
  timeout -k 30 "$verify_timeout" docker run --rm --platform linux/amd64 --network bridge \
    -v "$task_dir/tests:/tests:ro" \
    -v "$dir:/logs/verifier" \
    -v "$patch:/tmp/replay.patch:ro" \
    --entrypoint bash "$image" -c '
      cd /app 2>/dev/null || cd /testbed
      if [ -s /tmp/replay.patch ]; then
        { git apply --verbose /tmp/replay.patch || git apply --3way /tmp/replay.patch || patch --fuzz=3 -p1 -i /tmp/replay.patch; } > /logs/verifier/apply.log 2>&1
        echo "apply_exit=$?" >> /logs/verifier/apply.log
      else
        echo "empty patch" > /logs/verifier/apply.log
      fi
      bash /tests/test.sh' > "$dir/test-output.txt" 2>&1
}

log "grading on a fresh container"
verify_start=$(now)
grade "$out/model.patch" "$out/verifier"
verify_exit=$?
verify_seconds=$(( $(now) - verify_start ))
reward="$(cat "$out/verifier/reward.txt" 2>/dev/null || echo "")"
log "reward: ${reward:-none}"

if cmp -s "$out/model.patch" "$out/model.official.patch"; then
  patch_differs=False
  reward_official="$reward"
else
  patch_differs=True
  log "grading the official capture too"
  grade "$out/model.official.patch" "$out/verifier-official"
  reward_official="$(cat "$out/verifier-official/reward.txt" 2>/dev/null || echo "")"
  log "official reward: ${reward_official:-none}"
fi

python3 - "$out/result.json" <<PY
import json, sys
patch = open("$out/model.patch", errors="replace").read()
json.dump({
    "instance_id": "$instance_id",
    "image": "$image",
    "provider": "$provider",
    "model": "$model",
    "thinking": "$thinking",
    "resolved": "$reward".strip() == "1",
    "reward": "$reward".strip() or None,
    "resolved_official": "$reward_official".strip() == "1",
    "patch_differs_from_official": $patch_differs,
    "ignored_files_added": sum(1 for line in open("$out/agent/capture.log", errors="replace") if line.startswith("added ignored file")),
    "runtime_files_dropped": sum(1 for line in open("$out/agent/capture.log", errors="replace") if line.startswith("dropped runtime file")),
    "hidden_files_added": sum(1 for line in open("$out/agent/capture.log", errors="replace") if line.startswith(("added hidden file", "unhid index entry"))),
    "image_pulled": $( $pull_ok && echo True || echo False ),
    "kernel_bootstrap_ok": $( $bootstrap_ok && echo True || echo False ),
    "agent_exit_code": $agent_exit,
    "agent_timed_out": $( $timed_out && echo True || echo False ),
    "setup_seconds": $setup_seconds,
    "agent_seconds": $agent_seconds,
    "verify_exit_code": $verify_exit,
    "verify_seconds": $verify_seconds,
    "patch_bytes": len(patch.encode()),
    "patch_files": sum(1 for line in patch.splitlines() if line.startswith("diff --git ")),
    "repo_dir": "$repo_dir",
}, open(sys.argv[1], "w"), indent=2)
PY
cat "$out/result.json"
