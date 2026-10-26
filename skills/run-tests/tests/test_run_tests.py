import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

import run_tests  # noqa: E402


def test_go_module_runs_go_test(tmp_path):
    (tmp_path / "go.mod").write_text("module example\n")
    assert run_tests.detect_command(tmp_path, [], None) == ("go", ["go", "test", "./..."])
    assert run_tests.detect_command(tmp_path, ["./lib/..."], "TestLoad") == (
        "go",
        ["go", "test", "./lib/...", "-run", "TestLoad"],
    )


def test_package_json_picks_the_projects_runner(tmp_path):
    (tmp_path / "package.json").write_text(json.dumps({"devDependencies": {"jest": "29"}}))
    assert run_tests.detect_command(tmp_path, ["test/a.test.js"], "adds") == (
        "jest",
        ["npx", "jest", "test/a.test.js", "-t", "adds"],
    )
    (tmp_path / "package.json").write_text(json.dumps({"scripts": {"test": "node t.js"}}))
    assert run_tests.detect_command(tmp_path, [], None) == ("npm", ["npm", "test", "--"])


def test_python_projects_run_pytest(tmp_path):
    runner, argv = run_tests.detect_command(tmp_path, ["tests/test_x.py"], "parse")
    assert runner == "pytest"
    assert argv[1:] == ["-m", "pytest", "-q", "-rfE", "tests/test_x.py", "-k", "parse"]


def test_go_failures_are_named():
    output = "--- FAIL: TestLoad (0.00s)\n    --- FAIL: TestLoad/env (0.00s)\nFAIL\nFAIL\texample/config\t0.01s\n"
    report = run_tests.summarize("go", ["go", "test", "./..."], 1, 1.25, output)
    assert report.splitlines()[:5] == [
        "FAILED (exit 1) in 1.2s: go test ./...",
        "Failing (3):",
        "  TestLoad",
        "  TestLoad/env",
        "  example/config",
    ]


def test_pytest_failures_are_named_and_the_tail_kept():
    output = "\n".join(
        ["x" * 10] * 100
        + ["FAILED tests/test_a.py::test_one - assert 1 == 2", "1 failed, 3 passed in 0.10s"]
    )
    report = run_tests.summarize("pytest", ["python", "-m", "pytest"], 1, 0.1, output)
    assert "Failing (1):\n  tests/test_a.py::test_one" in report
    assert report.endswith("1 failed, 3 passed in 0.10s")
    assert len(report.splitlines()) < 50


def test_a_real_pytest_run_reports_its_failure(tmp_path):
    (tmp_path / "test_sample.py").write_text(
        "def test_ok():\n    assert True\n\n\ndef test_broken():\n    assert 1 == 2\n"
    )
    report = asyncio.run(
        run_tests.run(command=f"{sys.executable} -m pytest -q -rfE", cwd=str(tmp_path))
    )
    assert report.startswith("FAILED (exit 1)")
    assert "1 failed, 1 passed" in report


def test_a_hung_run_times_out(tmp_path):
    report = asyncio.run(run_tests.run(command="sleep 5", cwd=str(tmp_path), timeout=0.2))
    assert report.startswith("TIMED OUT")
