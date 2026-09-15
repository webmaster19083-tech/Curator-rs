#!/bin/sh
set -eu

# Tauri creates the DMG after applying the configured ad-hoc app signature.
# Verify both its integrity and that it can actually mount on the build host.
dmg=${1:?pass a DMG path}

if [ ! -f "$dmg" ]; then
  echo "DMG does not exist: $dmg" >&2
  exit 1
fi

hdiutil verify "$dmg"
device=$(hdiutil attach "$dmg" -readonly -nobrowse | awk '/^\/dev\// { print $1; exit }')
if [ -z "$device" ]; then
  echo "Could not mount DMG: $dmg" >&2
  exit 1
fi

cleanup() {
  hdiutil detach "$device" >/dev/null 2>&1 || true
}
trap cleanup EXIT HUP INT TERM

# The device node exists only after macOS has successfully attached the image.
if [ ! -e "$device" ]; then
  echo "Mounted DMG device is unavailable: $device" >&2
  exit 1
fi
