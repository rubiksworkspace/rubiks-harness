#!/bin/sh
# Installs the Rubiks build of Prime Agent from this disk image: the release
# archive beside this script, through the same installer the release uses.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
archive="$(ls "$here"/prime-agent-*-darwin-arm64.tar.gz 2>/dev/null | head -n 1)"
if [ -z "$archive" ]; then
  echo "No Prime Agent archive found next to this script." >&2
  exit 1
fi
if [ "$(uname -m)" != "arm64" ]; then
  echo "This build is for Apple silicon Macs only." >&2
  exit 1
fi

sh "$here/install-rust.sh" --archive "$archive"

echo
echo "Prime Agent (Rubiks build) is installed. Open a new terminal and run: prime-agent"
printf "Press Return to close this window. "
read -r _
