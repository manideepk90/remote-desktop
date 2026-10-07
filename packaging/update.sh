#!/bin/sh
# Rebuild Remote Desk and restart the running service so it picks up the new binary.
#
#   packaging/update.sh             per-user install (the service runs target/release/remote-desk)
#   packaging/update.sh --system    system-wide install in /usr/local (asks for sudo)
set -eu

here="$(cd "$(dirname "$0")/.." && pwd)"
bin="$here/target/release/remote-desk"

echo "==> Building"
cargo build --release --manifest-path "$here/Cargo.toml"

if [ "${1:-}" = "--system" ]; then
    echo "==> Installing system-wide"
    sudo "$here/packaging/install-system.sh"
else
    echo "==> Refreshing the menu entry and user service"
    if systemctl --user is-enabled --quiet remote-desk.service; then
        "$bin" install --autostart
    else
        "$bin" install
    fi
fi

systemctl --user daemon-reload
if systemctl --user is-enabled --quiet remote-desk.service || systemctl --user is-active --quiet remote-desk.service; then
    echo "==> Restarting remote-desk.service"
    systemctl --user restart remote-desk.service
    sleep 2
    if systemctl --user is-active --quiet remote-desk.service; then
        echo "Running. Recent log:"
        journalctl --user -u remote-desk.service -n 5 --no-pager -o cat
    else
        echo "The service failed to start. See: journalctl --user -u remote-desk.service -e" >&2
        exit 1
    fi
else
    echo "The service is not enabled. Start it with:  systemctl --user enable --now remote-desk.service"
fi
