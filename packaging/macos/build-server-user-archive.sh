#!/bin/sh
set -eu

# Build the current-user Server archive. Unlike the all-users .pkg, this
# bundle carries a LaunchAgent template and a non-elevated installer script.
binary=${1:?pass the compiled curator Server binary}
output=${2:?pass an output .tar.gz path}
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)

stage=$(mktemp -d "${TMPDIR:-/tmp}/curator-server-user.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM
mkdir -p "$stage/static"
install -m 0755 "$binary" "$stage/curator"
cp -R "$root/static/." "$stage/static/"
install -m 0644 "$root/packaging/macos/tech.webmaster19083.curator.server.user.plist" \
  "$stage/tech.webmaster19083.curator.server.user.plist"
install -m 0755 "$root/packaging/macos/install-current-user.sh" "$stage/install-current-user.sh"

mkdir -p "$(dirname -- "$output")"
tar -C "$stage" -czf "$output" .
tar -tzf "$output" >/dev/null
