#!/bin/sh
set -eu

# Build an all-users Debian package plus a portable archive from an already
# compiled Server binary. Current-user installs use the archive and the user
# systemd unit; the .deb provides the system service unit as well.
binary=${1:?pass the compiled curator Server binary}
output=${2:?pass an output directory}
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
stage=$(mktemp -d "${TMPDIR:-/tmp}/curator-server-deb.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM

mkdir -p "$output" "$stage/DEBIAN" "$stage/usr/lib/curator/static" "$stage/lib/systemd/system" "$stage/usr/lib/systemd/user"
install -m 0755 "$binary" "$stage/usr/lib/curator/curator"
cp -R "$root/static/." "$stage/usr/lib/curator/static/"
install -m 0644 "$root/packaging/linux/curator-server.service" "$stage/lib/systemd/system/curator-server.service"
sed 's|__CURATOR_SERVER_PATH__|%h/.local/lib/curator/curator|g' \
  "$root/packaging/linux/curator-server-user.service" > "$stage/usr/lib/systemd/user/curator-server-user.service"
chmod 0644 "$stage/usr/lib/systemd/user/curator-server-user.service"
sed "s/@CURATOR_VERSION@/$version/" "$root/packaging/linux/debian-control" > "$stage/DEBIAN/control"
install -m 0755 "$root/packaging/linux/postinst" "$stage/DEBIAN/postinst"
install -m 0755 "$root/packaging/linux/prerm" "$stage/DEBIAN/prerm"

dpkg-deb --root-owner-group --build "$stage" "$output/curator-server_${version}_amd64.deb"

# The portable archive is the current-user package. Include exactly the
# current-user unit and installer alongside the binary/static resources rather
# than leaving someone to hunt through a system package for service files.
portable=$(mktemp -d "${TMPDIR:-/tmp}/curator-server-portable.XXXXXX")
trap 'rm -rf "$stage" "$portable"' EXIT HUP INT TERM
mkdir -p "$portable/static"
install -m 0755 "$binary" "$portable/curator"
cp -R "$root/static/." "$portable/static/"
install -m 0644 "$root/packaging/linux/curator-server-user.service" "$portable/curator-server-user.service"
install -m 0755 "$root/packaging/linux/install-current-user.sh" "$portable/install-current-user.sh"
tar -C "$portable" -czf "$output/curator-server-${version}-linux-x86_64.tar.gz" .
