#!/bin/sh
set -eu

app=${1:?pass a .app bundle}
output_dir=${2:?pass an output directory}
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

mkdir -p "$output_dir"
name=$(basename "$app" .app)
output="$output_dir/${name}-${version}.dmg"

# This intentionally uses an ad-hoc identity. It proves bundle integrity on
# the build machine but is not notarization and does not bypass Gatekeeper.
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict --verbose=2 "$app"
hdiutil create -volname "$name" -srcfolder "$app" -ov -format UDZO "$output"
hdiutil verify "$output"

# Mount once as a packaging smoke check. hdiutil's own output includes the
# device node first; detaching it avoids leaving CI volumes mounted.
device=$(hdiutil attach "$output" -readonly -nobrowse | awk 'NR == 1 { print $1 }')
if [ -z "$device" ]; then
  echo "Could not mount generated DMG" >&2
  exit 1
fi
hdiutil detach "$device"
