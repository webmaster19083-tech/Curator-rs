#!/bin/sh
set -eu

# Verify the parts of a finished app bundle that are easy to lose when a
# release job changes targets or resource layout. The caller supplies the
# expected bundle identity and thin-binary architecture for this matrix row.
app=${1:?pass a .app bundle}
expected_identifier=${2:?pass the expected bundle identifier}
expected_arch=${3:?pass the expected Mach-O architecture}

plist="$app/Contents/Info.plist"
if [ ! -f "$plist" ] || [ ! -d "$app/Contents/Resources" ]; then
  echo "Incomplete macOS app bundle: $app" >&2
  exit 1
fi

identifier=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$plist")
if [ "$identifier" != "$expected_identifier" ]; then
  echo "Expected $expected_identifier, found $identifier in $app" >&2
  exit 1
fi

executable=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$plist")
binary="$app/Contents/MacOS/$executable"
if [ ! -x "$binary" ]; then
  echo "App executable is missing or not executable: $binary" >&2
  exit 1
fi
if ! lipo -archs "$binary" | tr ' ' '\n' | grep -Fx "$expected_arch" >/dev/null; then
  echo "Expected $expected_arch executable architecture in $binary" >&2
  exit 1
fi

# A Tauri app with no payload resources cannot serve its local shell. Do not
# depend on an internal Tauri resource filename: just require a non-empty
# Resources directory in addition to the signed app and public bundle ID.
if ! find "$app/Contents/Resources" -type f -print -quit | grep -q .; then
  echo "No bundled resources found in $app" >&2
  exit 1
fi

codesign --verify --deep --strict --verbose=2 "$app"
