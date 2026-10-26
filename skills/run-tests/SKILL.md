---
name: run-tests
description: Run the repository's tests (go test, pytest, jest, vitest, mocha or npm test, detected from the project) and get a short summary of what failed instead of the whole log.
---

# Run Tests

Run the tests that cover a change and read only what matters: pass or fail,
the failing tests, and the end of the log.

    await run_tests()                                   # the whole suite
    await run_tests(["tests/test_parser.py"])           # specific files or packages
    await run_tests(["./lib/config/..."], pattern="TestLoad")   # go -run / pytest -k / jest -t
    await run_tests(command="make test-unit")           # your own command

The runner is detected from the repository: `go.mod` runs `go test`, a Python
project runs `python -m pytest`, and `package.json` runs jest, vitest or mocha
when the project depends on one, otherwise `npm test`. Tests run in the
project's own environment (the `python`, `go` and `npx` on PATH), not in the
kernel's.

Returns a short report: the command, its exit code and duration, the failing
tests the runner named, and the last lines of the output. `timeout` (seconds,
default 600) stops a hung run.
