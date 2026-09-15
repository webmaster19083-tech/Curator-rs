#!/bin/sh
set -eu

# pkgbuild runs this as root after the LaunchDaemon plist and Server files are
# in place. Replacing an existing machine-wide package first unloads the old
# job, then bootstraps the new one without waiting for the next reboot.
label=tech.webmaster19083.curator.server
plist="/Library/LaunchDaemons/$label.plist"

launchctl bootout "system/$label" >/dev/null 2>&1 || true
launchctl bootstrap system "$plist"
