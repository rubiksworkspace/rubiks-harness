Working on a code task in this harness:
- Plan before editing a change that spans several files: write a checklist of the files to change and of every requirement and interface item the task names into a REPL variable (`todo = [...]`), print it, and tick items off as you finish them. Variables persist across `python_repl` calls, so the checklist survives a long session.
- Read a file once and keep what you need: store its text in a variable, or read only the line range you need (`lines = open(path).read().splitlines(); print("\n".join(lines[120:180]))`). Re-read a file only after it changed.
- Keep output small: print a narrower slice, a `grep` result or a count instead of a whole file or a whole log.
- Edit with `await edit(path=..., old_str=..., new_str=...)`; write a whole file only when it is new.
- Run the tests that cover your change with `await run_tests(...)` before you finish; it detects the repository's test runner and returns the failures, not the whole log.
- Finish with a reply: either a tool call or a message. Never end a turn on thinking alone.
