//! Runs one task as a child `prime-agent -p --mode json` process.

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{sleep_until, timeout, Instant};

use crate::events::Translator;
use crate::store::{Outcome, SessionRecord, Store, Task, TaskResult};

/// Between asking the task's process group to stop and forcing it.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long stdout may stay open after the child exited, which only happens
/// when something it started still holds the pipe.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(500);
/// How much of the child's stderr is kept for the server log.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

pub struct RunSpec {
    pub task: Arc<Task>,
    pub store: Arc<Store>,
    pub executable: PathBuf,
    pub token_env: String,
    pub session: SessionRecord,
    pub prompt: String,
    pub instructions: Option<String>,
    pub provider: String,
    pub model: String,
    /// Do not persist a Prime Agent session: this task's data must not outlive it.
    pub ephemeral: bool,
    pub max_step: Option<u64>,
    pub timeout: Option<Duration>,
}

/// Why the server stopped a task early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    Interrupted,
    Timeout,
    MaxSteps,
}

fn child_command(spec: &RunSpec) -> Command {
    let mut command = Command::new(&spec.executable);
    command.args([
        "-p",
        "--mode",
        "json",
        "--provider",
        &spec.provider,
        "--model",
        &spec.model,
    ]);
    if let Some(resume) = &spec.session.prime_session_id {
        command.args(["--resume", resume]);
    } else if spec.ephemeral {
        command.arg("--no-session");
    }
    if let Some(instructions) = &spec.instructions {
        command.args(["--append-system-prompt", instructions]);
    }
    command
        .current_dir(&spec.session.workspace)
        .env_remove(&spec.token_env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    command
}

/// The group is addressed by the pid saved at spawn: a reaped child no longer
/// reports one, and what it started may still be running.
#[cfg(unix)]
fn signal_group(group: Option<u32>, signal: nix::sys::signal::Signal) {
    let Some(group) = group.and_then(|id| i32::try_from(id).ok()) else {
        return;
    };
    // The child leads its own group, so its tools' processes get this too. A
    // group that is already gone is the outcome being asked for.
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), signal);
}

/// Asks the child to stop, then forces it, and returns once it is gone.
async fn stop_child(child: &mut Child, group: Option<u32>) -> Option<ExitStatus> {
    #[cfg(unix)]
    signal_group(group, nix::sys::signal::Signal::SIGTERM);
    #[cfg(not(unix))]
    let _ = (child.start_kill(), group);
    if let Ok(status) = timeout(STOP_GRACE, child.wait()).await {
        return status.ok();
    }
    #[cfg(unix)]
    signal_group(group, nix::sys::signal::Signal::SIGKILL);
    let _ = child.start_kill();
    child.wait().await.ok()
}

async fn read_stderr_tail(mut stderr: impl AsyncRead + Unpin) -> String {
    let mut tail = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(read) = stderr.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..read]);
        if tail.len() > STDERR_TAIL_BYTES {
            tail.drain(..tail.len() - STDERR_TAIL_BYTES);
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

/// Maps how the run ended to what the response reports. Failure messages stay
/// generic: errors must carry no paths or credentials, and a provider's or
/// child's own text can hold either. The server log has the detail.
fn outcome_of(stop: Option<Stop>, exited_ok: bool, translator: &Translator) -> Outcome {
    match stop {
        Some(Stop::Cancelled) => Outcome::Cancelled,
        Some(Stop::Interrupted) => Outcome::Incomplete {
            reason: "interrupted",
        },
        Some(Stop::Timeout) => Outcome::Incomplete { reason: "timeout" },
        Some(Stop::MaxSteps) => Outcome::Incomplete {
            reason: "max_steps",
        },
        None if translator.ended_on_provider_error() => Outcome::Failed {
            code: "provider_error",
            message: "The model provider returned an error.".to_string(),
        },
        None if exited_ok => Outcome::Completed,
        None => Outcome::Failed {
            code: "harness_error",
            message: "The harness stopped unexpectedly.".to_string(),
        },
    }
}

fn result_of(spec: &RunSpec, translator: &Translator, outcome: Outcome) -> TaskResult {
    TaskResult {
        outcome,
        usage: translator.usage_json(),
        ran_model: translator
            .model()
            .filter(|ran| *ran != spec.model)
            .map(String::from),
    }
}

fn spawn_failure() -> TaskResult {
    TaskResult {
        outcome: Outcome::Failed {
            code: "harness_unavailable",
            message: "The harness could not be started.".to_string(),
        },
        usage: Value::Null,
        ran_model: None,
    }
}

/// Runs the task to its end. Owns the task's whole life, independently of any
/// connection, so a client going away never stops the work.
#[allow(clippy::too_many_lines)]
pub async fn run(spec: RunSpec) {
    let task = Arc::clone(&spec.task);
    let mut child = match child_command(&spec).spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("uhp: {} could not start: {error}", task.id);
            spec.store.finish(&task, spawn_failure());
            return;
        }
    };
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        spec.store.finish(&task, spawn_failure());
        return;
    };
    let prompt = spec.prompt.clone();
    // On its own task: a prompt larger than the pipe buffer must not block
    // reading the child's output. Dropping stdin afterwards is the end of input.
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
    });
    let stderr_tail = tokio::spawn(read_stderr_tail(stderr));

    let group = child.id();
    let mut lines = BufReader::new(stdout).lines();
    let mut translator = Translator::default();
    let mut session = spec.session.clone();
    let deadline = spec.timeout.map(|budget| Instant::now() + budget);
    let mut stop = None;
    let mut exit_status = None;
    let mut drain_until = None;
    let mut stdout_open = true;
    while stdout_open || exit_status.is_none() {
        if spec
            .max_step
            .is_some_and(|max_step| translator.step_budget_spent(max_step))
        {
            stop = stop.or(Some(Stop::MaxSteps));
        }
        if stop.is_some() {
            exit_status = stop_child(&mut child, group).await.or(exit_status);
            break;
        }
        tokio::select! {
            line = lines.next_line(), if stdout_open => {
                let Ok(Some(line)) = line else {
                    stdout_open = false;
                    continue;
                };
                let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
                task.publish(translator.feed(&event));
                remember_prime_session(&spec, &mut session, &translator);
            }
            status = child.wait(), if exit_status.is_none() => {
                exit_status = status.ok();
                drain_until = Some(Instant::now() + DRAIN_AFTER_EXIT);
            }
            () = task.cancel.cancelled() => {
                stop = Some(if task.was_interrupted() {
                    Stop::Interrupted
                } else {
                    Stop::Cancelled
                });
            }
            () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                stop = Some(Stop::Timeout);
            }
            () = sleep_until(drain_until.unwrap_or_else(Instant::now)), if drain_until.is_some() => {
                #[cfg(unix)]
                signal_group(group, nix::sys::signal::Signal::SIGKILL);
                break;
            }
        }
    }
    task.publish(translator.finish());

    let exited_ok = exit_status.is_some_and(|status| status.success());
    let outcome = outcome_of(stop, exited_ok, &translator);
    if matches!(outcome, Outcome::Failed { .. }) {
        let detail = translator.last_error().unwrap_or("");
        // A child that outlived the stop can hold stderr open; never wait on it.
        let tail = timeout(DRAIN_AFTER_EXIT, stderr_tail)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        eprintln!("uhp: {} failed: {detail}\n{tail}", task.id);
    }
    let result = result_of(&spec, &translator, outcome);
    spec.store.finish(&task, result);
    if spec.ephemeral {
        let _ = std::fs::remove_dir_all(spec.store.session_workspace(&task.session_id));
    }
}

/// Records which Prime Agent session this one resumes, as soon as it is known,
/// so a task that dies later still leaves the session continuable.
fn remember_prime_session(spec: &RunSpec, session: &mut SessionRecord, translator: &Translator) {
    let Some(prime_id) = translator.prime_session_id() else {
        return;
    };
    if spec.ephemeral || session.prime_session_id.as_deref() == Some(prime_id) {
        return;
    }
    session.prime_session_id = Some(prime_id.to_string());
    if let Err(error) = spec.store.save_session(session) {
        eprintln!("uhp: could not save {}: {error:#}", session.id);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::store::NewTask;
    use serde_json::json;

    fn failed_code(outcome: &Outcome) -> &'static str {
        match outcome {
            Outcome::Failed { code, .. } => code,
            other => panic!("{other:?}"),
        }
    }

    /// Runs a task with a shell script standing in for `prime-agent`.
    fn script_spec(dir: &std::path::Path, script: &str, token_env: &str) -> RunSpec {
        use std::os::unix::fs::PermissionsExt;
        let executable = dir.join("fake-agent");
        std::fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = Arc::new(Store::open(&dir.join("state")).unwrap());
        let session = SessionRecord {
            id: "hsesstest".to_string(),
            prime_session_id: Some("prime-id".to_string()),
            workspace: dir.to_path_buf(),
        };
        let task = store
            .begin(NewTask {
                session_id: session.id.clone(),
                previous_response_id: None,
                model: "m".to_string(),
                store: true,
                metadata: serde_json::Map::new(),
                input_items: Vec::new(),
            })
            .unwrap();
        RunSpec {
            task,
            store,
            executable,
            token_env: token_env.to_string(),
            session,
            prompt: "--help @/etc/hosts".to_string(),
            instructions: Some("-be brief".to_string()),
            provider: "p".to_string(),
            model: "m".to_string(),
            ephemeral: false,
            max_step: None,
            timeout: None,
        }
    }

    fn process_exists(pid: i32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    #[tokio::test]
    async fn the_child_gets_its_arguments_the_prompt_on_stdin_and_not_the_token() {
        let dir = tempfile::tempdir().unwrap();
        // HOME stands in for the token variable: it is always set in the parent.
        let spec = script_spec(
            dir.path(),
            r#"printf '%s\n' "$@" > args
cat > prompt
printf '[%s]' "$HOME" > token"#,
            "HOME",
        );
        let task = Arc::clone(&spec.task);
        run(spec).await;
        assert_eq!(
            task.response()["status"],
            "completed",
            "{}",
            task.response()
        );
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "-p",
                "--mode",
                "json",
                "--provider",
                "p",
                "--model",
                "m",
                "--resume",
                "prime-id",
                "--append-system-prompt",
                "-be brief",
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("prompt")).unwrap(),
            "--help @/etc/hosts"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("token")).unwrap(),
            "[]"
        );
    }

    #[tokio::test]
    async fn cancelling_kills_everything_the_task_started() {
        let dir = tempfile::tempdir().unwrap();
        // TERM is ignored by both, so this also proves the escalation to KILL.
        let spec = script_spec(
            dir.path(),
            r#"trap '' TERM
sleep 300 &
echo $! > grandchild
echo '{"type":"session","id":"prime-id"}'
wait"#,
            "UHP_TEST_NO_SUCH_VARIABLE",
        );
        let task = Arc::clone(&spec.task);
        let running = tokio::spawn(run(spec));
        let path = dir.path().join("grandchild");
        let grandchild: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(process_exists(grandchild));
        task.cancel.cancel();
        timeout(Duration::from_secs(10), running)
            .await
            .expect("the run ends")
            .unwrap();
        assert_eq!(task.response()["status"], "cancelled");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!process_exists(grandchild), "the grandchild survived");
    }

    #[tokio::test]
    async fn a_child_that_leaves_something_holding_stdout_does_not_hang_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let spec = script_spec(
            dir.path(),
            r#"sleep 300 &
echo '{"type":"session","id":"prime-id"}'
exit 0"#,
            "UHP_TEST_NO_SUCH_VARIABLE",
        );
        let task = Arc::clone(&spec.task);
        timeout(Duration::from_secs(10), run(spec))
            .await
            .expect("the run ends");
        assert_eq!(task.response()["status"], "completed");
    }

    #[tokio::test]
    async fn a_binary_that_cannot_start_fails_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = script_spec(dir.path(), "exit 0", "UHP_TEST_NO_SUCH_VARIABLE");
        spec.executable = dir.path().join("no-such-binary");
        let task = Arc::clone(&spec.task);
        run(spec).await;
        assert_eq!(task.response()["status"], "failed");
        assert_eq!(task.response()["error"]["code"], "harness_unavailable");
    }

    #[test]
    fn a_stop_decides_the_outcome_whatever_the_exit_code() {
        let translator = Translator::default();
        assert_eq!(
            outcome_of(Some(Stop::Cancelled), false, &translator),
            Outcome::Cancelled
        );
        assert_eq!(
            outcome_of(Some(Stop::Timeout), false, &translator),
            Outcome::Incomplete { reason: "timeout" }
        );
        assert_eq!(
            outcome_of(Some(Stop::MaxSteps), true, &translator),
            Outcome::Incomplete {
                reason: "max_steps"
            }
        );
        assert_eq!(
            outcome_of(Some(Stop::Interrupted), false, &translator),
            Outcome::Incomplete {
                reason: "interrupted"
            }
        );
    }

    #[test]
    fn a_clean_exit_completes_and_a_bad_one_is_a_harness_error() {
        let translator = Translator::default();
        assert_eq!(outcome_of(None, true, &translator), Outcome::Completed);
        assert_eq!(
            failed_code(&outcome_of(None, false, &translator)),
            "harness_error"
        );
    }

    #[test]
    fn a_provider_error_is_named_without_repeating_its_text() {
        let mut translator = Translator::default();
        translator.feed(&json!({ "type": "message_end", "message": {
            "role": "assistant", "content": [], "stopReason": "error",
            "errorMessage": "secret sk-123 at /home/me",
        }}));
        for exited_ok in [true, false] {
            let outcome = outcome_of(None, exited_ok, &translator);
            assert_eq!(failed_code(&outcome), "provider_error");
            let Outcome::Failed { message, .. } = outcome else {
                unreachable!()
            };
            assert!(!message.contains("sk-123"));
        }
    }
}
