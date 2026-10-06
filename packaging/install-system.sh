#!/bin/sh
# System-wide install (run with sudo). Puts the binary in a root-owned location
# and registers the KWin access grant and the user service for every account.
set -eu

PREFIX="${PREFIX:-/usr/local}"
here="$(cd "$(dirname "$0")/.." && pwd)"
bin="$here/target/release/remote-desk"

if [ "$(id -u)" -ne 0 ]; then
    echo "Run this with sudo: sudo $0" >&2
    exit 1
fi
if [ ! -x "$bin" ]; then
    echo "Build first: cargo build --release" >&2
    exit 1
fi

install -Dm755 "$bin" "$PREFIX/bin/remote-desk"

"$PREFIX/bin/remote-desk" print-desktop-entry > /usr/share/applications/dev.remotedesk.RemoteDesk.desktop
chmod 644 /usr/share/applications/dev.remotedesk.RemoteDesk.desktop

"$PREFIX/bin/remote-desk" print-service-unit > /usr/lib/systemd/user/remote-desk.service
chmod 644 /usr/lib/systemd/user/remote-desk.service

echo "Installed $PREFIX/bin/remote-desk"
echo "Each user can now run:  systemctl --user enable --now remote-desk.service"
echo "Remove any per-user copy with:  remote-desk uninstall  (run as that user, before this install)"
