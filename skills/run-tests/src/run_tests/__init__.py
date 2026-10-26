"""Run the repository's tests and summarize what failed."""

from __future__ import annotations

import asyncio
import json
import re
import shlex
import shutil
import time
from pathlib import Path

# Lines of the log's end the report keeps.
_TAIL_LINES = 40
# Characters the report's tail may take.
_TAIL_CHARS = 4000
# Failing test names the report lists.
_MAX_FAILURES = 30

_FAILURE_PATTERNS = {
    "go": re.compile(r"^[ \t]*--- FAIL: (\S+)|^FAIL[ \t]+(\S+)", re.M),
    "pytest": re.compile(r"^(?:FAILED|ERROR) (\S+)", re.M),
    "jest": re.compile(r"^[ \t]*● (.+?)[ \t]*$", re.M),
    "vitest": re.compile(r"^[ \t]*(?:FAIL|×)[ \t]+(.+?)[ \t]*$", re.M),
    "mocha": re.compile(r"^[ \t]*\d+\) (.+?)[ \t]*$", re.M),
}


async def run(
    paths: list[str] | str | None = None,
    *,
    pattern: str | None = None,
    command: str | None = None,
    cwd: str = ".",
    timeout: float = 600,
) -> str:
    """Run the tests and return a short report.

    Args:
        paths: Test files, directories or Go packages to run; the whole
            suite when omitted.
        pattern: Only tests matching this name (go ``-run``, pytest ``-k``,
            jest/vitest ``-t``, mocha ``-g``).
        command: Run this shell command instead of the detected runner.
        cwd: The repository root.
        timeout: Seconds before the run is stopped.

    Returns:
        The command, exit code, duration, failing tests and the log's end.
    """
    root = Path(cwd).expanduser()
    if isinstance(paths, str):
        paths = [paths]
    if command is not None:
        runner, argv = "custom", ["sh", "-c", command]
    else:
        runner, argv = detect_command(root, list(paths or []), pattern)
    started = time.monotonic()
    process = await asyncio.create_subprocess_exec(
        *argv,
        cwd=root,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )
    try:
        output, _ = await asyncio.wait_for(process.communicate(), timeout)
        exit_code = process.returncode
    except asyncio.TimeoutError:
        process.kill()
        output, _ = await process.communicate()
        exit_code = None
    return summarize(
        runner,
        argv,
        exit_code,
        time.monotonic() - started,
        output.decode(errors="replace"),
    )


def detect_command(
    root: Path, paths: list[str], pattern: str | None
) -> tuple[str, list[str]]:
    """The runner name and argv for the repository at ``root``."""
    if (root / "go.mod").is_file():
        argv = ["go", "test", *(paths or ["./..."])]
        if pattern:
            argv += ["-run", pattern]
        return "go", argv
    package = root / "package.json"
    if package.is_file():
        try:
            manifest = json.loads(package.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            manifest = {}
        deps = {
            **manifest.get("dependencies", {}),
            **manifest.get("devDependencies", {}),
        }
        for runner, base, flag in (
            ("jest", ["npx", "jest"], "-t"),
            ("vitest", ["npx", "vitest", "run"], "-t"),
            ("mocha", ["npx", "mocha"], "-g"),
        ):
            if runner in deps:
                argv = [*base, *paths]
                if pattern:
                    argv += [flag, pattern]
                return runner, argv
        return "npm", ["npm", "test", "--", *paths]
    python = shutil.which("python") or shutil.which("python3") or "python3"
    argv = [python, "-m", "pytest", "-q", "-rfE", *paths]
    if pattern:
        argv += ["-k", pattern]
    return "pytest", argv


def summarize(
    runner: str,
    argv: list[str],
    exit_code: int | None,
    seconds: float,
    output: str,
) -> str:
    """The report for one finished (or timed-out) run."""
    if exit_code is None:
        verdict = "TIMED OUT"
    elif exit_code == 0:
        verdict = "PASSED"
    else:
        verdict = f"FAILED (exit {exit_code})"
    lines = [f"{verdict} in {seconds:.1f}s: {shlex.join(argv)}"]
    failures = _failures(runner, output)
    if failures:
        shown = failures[:_MAX_FAILURES]
        lines.append(f"Failing ({len(failures)}):")
        lines += [f"  {name}" for name in shown]
        if len(failures) > len(shown):
            lines.append(f"  ... and {len(failures) - len(shown)} more")
    tail = "\n".join(output.rstrip().splitlines()[-_TAIL_LINES:])
    if len(tail) > _TAIL_CHARS:
        tail = tail[-_TAIL_CHARS:]
    if tail:
        lines += ["Last lines of the output:", tail]
    return "\n".join(lines)


def _failures(runner: str, output: str) -> list[str]:
    pattern = _FAILURE_PATTERNS.get(runner)
    if pattern is None:
        return []
    names: list[str] = []
    for match in pattern.finditer(output):
        name = next(group for group in match.groups() if group)
        if name not in names:
            names.append(name)
    return names
