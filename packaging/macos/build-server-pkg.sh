#!/bin/sh
set -eu

# The resulting Server .pkg is deliberately unsigned. Host/Viewer apps and
# the Server executable are ad-hoc signed by the release workflow; signing a
# pkg itself requires an Apple Installer certificate that this project does
# not claim to possess.
binary=${1:?pass the compiled curator Server binary}
output=${2:?pass an output .pkg path}
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
version=$(awk '
  /^\[workspace.package\]$/ { in_section=1; next }
  in_section && /^\[/ { exit }
  in_section && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "$root/Cargo.toml")
if [ -z "$version" ]; then
  echo "Could not read workspace version from Cargo.toml" >&2
  exit 1
fi
stage=$(mktemp -d "${TMPDIR:-/tmp}/curator-server-pkg.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM
scripts="$stage-scripts"
trap 'rm -rf "$stage" "$scripts"' EXIT HUP INT TERM

mkdir -p "$stage/Library/Application Support/Curator/static" "$stage/Library/LaunchDaemons"
mkdir -p "$scripts"
install -m 0755 "$binary" "$stage/Library/Application Support/Curator/curator"
cp -R "$root/static/." "$stage/Library/Application Support/Curator/static/"
install -m 0644 "$root/packaging/macos/tech.webmaster19083.curator.server.plist" "$stage/Library/LaunchDaemons/tech.webmaster19083.curator.server.plist"
install -m 0755 "$root/packaging/macos/postinstall-server.sh" "$scripts/postinstall"
pkgbuild --root "$stage" --scripts "$scripts" --identifier tech.webmaster19083.curator.server --version "$version" "$output"
