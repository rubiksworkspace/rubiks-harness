Prime Agent — Rubiks build
==========================

This disk image installs the Rubiks build of Prime Agent (the rubiks-harness
gpl-3.0 branch) on an Apple silicon Mac. It differs from upstream Prime Agent:

  - Telemetry is off unless you turn it on, and is never sent anywhere.
  - A local research log is kept under ~/.prime/agent/research
    (see `prime-agent data research`); it never leaves the machine.
  - The Rubiks themes and logo.

Install
-------
Double-click "Install Prime Agent". The first time, macOS may say it is from
an unidentified developer: right-click it, choose Open, then Open again.

Or, in Terminal:

  sh "/Volumes/Prime Agent (Rubiks)/install-rust.sh" --archive "/Volumes/Prime Agent (Rubiks)/"prime-agent-*-darwin-arm64.tar.gz

It installs into ~/.local (the `prime-agent` command lands in ~/.local/bin).

Updating
--------
Install a newer copy of this disk image. Do not run `prime-agent update`:
that installs upstream Prime Agent in place of the Rubiks build.
