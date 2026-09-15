#!/bin/sh
set -eu

# Install the portable Server bundle for the invoking macOS user. This keeps
# the LaunchAgent and Application Support data in that user's profile and
# never needs elevation.
bundle=${1:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}
target=${CURATOR_SERVER_HOME:-"$HOME/Library/Application Support/Curator"}
launch_agents="$HOME/Library/LaunchAgents"
label=tech.webmaster19083.curator.server.user
plist="$launch_agents/$label.plist"

if [ ! -x "$bundle/curator" ] || [ ! -d "$bundle/static" ]; then
  echo "This does not look like a Curator Server portable bundle." >&2
  exit 1
fi

mkdir -p "$target" "$launch_agents"
cp -R "$bundle/." "$target/"
escaped_target=$(printf '%s' "$target/curator" | sed 's/[&|]/\\&/g')
sed "s|__CURATOR_SERVER_PATH__|$escaped_target|g" \
  "$target/tech.webmaster19083.curator.server.user.plist" > "$plist"

uid=$(id -u)
launchctl bootout "gui/$uid" "$plist" >/dev/null 2>&1 || true
launchctl bootstrap "gui/$uid" "$plist"
echo "Curator Server is running for the current user. Open http://127.0.0.1:42168/"
