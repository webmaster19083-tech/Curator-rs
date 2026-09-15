#!/bin/sh
set -eu

# Install a portable Curator Server archive for the current user. The archive
# deliberately has no privileged post-install action: this copies files into
# the user's local application directory and registers a systemd --user unit.
bundle=${1:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}
target=${CURATOR_SERVER_HOME:-"$HOME/.local/lib/curator"}
unit_dir=${XDG_CONFIG_HOME:-"$HOME/.config"}/systemd/user

if [ ! -x "$bundle/curator" ] || [ ! -d "$bundle/static" ]; then
  echo "This does not look like a Curator Server portable bundle." >&2
  exit 1
fi

mkdir -p "$target" "$unit_dir"
cp -R "$bundle/." "$target/"
escaped_target=$(printf '%s' "$target/curator" | sed 's/[\\&|]/\\&/g')
sed "s|__CURATOR_SERVER_PATH__|$escaped_target|g" \
  "$bundle/curator-server-user.service" > "$unit_dir/curator-server-user.service"
chmod 0644 "$unit_dir/curator-server-user.service"
systemctl --user daemon-reload
systemctl --user enable --now curator-server-user.service

echo "Curator Server is running for the current user. Open http://127.0.0.1:42168/"
