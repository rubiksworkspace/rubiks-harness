#!/bin/sh
# Install Prime Agent (Rubiks), the harness build the Rubiks IDE runs, from
# its newest GitHub release:
#
#   curl -fsSL https://github.com/rubiksworkspace/rubiks-harness/releases/latest/download/install.sh | sh
#
# The release workflow (.github/workflows/rubiks-dmg.yml) publishes this
# script with the release archive's name filled in; the archive's own
# installer (install-rust.sh --archive) does the install, so the layout is
# the official one: the launcher at ~/.local/bin/prime-agent, the payload
# under ~/.local/share/prime-agent. Extra arguments go to install-rust.sh
# (for example --force). Non-interactive.
set -eu

RELEASE_URL="${PRIME_AGENT_RUBIKS_RELEASE_URL:-https://github.com/rubiksworkspace/rubiks-harness/releases/latest/download}"
ARCHIVE="__RUBIKS_ARCHIVE__"

die() { echo "install.sh: $1" >&2; exit 1; }

case "$ARCHIVE" in
  __*) die "this copy has no archive name; use the one published with a release" ;;
esac
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] \
  || die "Prime Agent (Rubiks) is published for Apple silicon Macs only"
command -v curl > /dev/null 2>&1 || die "curl is required"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading Prime Agent (Rubiks): $ARCHIVE" >&2
curl -fsSL "$RELEASE_URL/$ARCHIVE" -o "$tmp/$ARCHIVE" \
  || die "could not download $RELEASE_URL/$ARCHIVE"
curl -fsSL "$RELEASE_URL/install-rust.sh" -o "$tmp/install-rust.sh" \
  || die "could not download $RELEASE_URL/install-rust.sh"
sh "$tmp/install-rust.sh" --archive "$tmp/$ARCHIVE" "$@"
