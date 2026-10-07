# Remote Desk

A VNC server for **KDE Plasma on Wayland**, written from scratch in Rust. Connect to your desktop from any VNC app (RealVNC Viewer, TigerVNC, Remmina, bVNC, Screens, …) and approve new devices from a desktop notification.

https://github.com/user-attachments/assets/a42deb5c-6087-4afc-9107-37c50abc141c

## Features

- **View and control:** full keyboard and mouse, with view-only available globally or per session.
- **Resolution control:** share one monitor or all of them, scaled 100% / 75% / 50% / 33% on the GPU by KWin, with a frame-rate cap. Changes apply live and connected clients resize without reconnecting.
- **Virtual monitor:** share an extra monitor that only remote devices see (720p up to 4K, or a custom size) instead of your real ones. If every monitor is unplugged, Remote Desk switches to a virtual monitor on its own so the session keeps working.
- **Pairing:**
  - After the VNC password, an unknown device triggers a notification: **Allow once / Always allow / Deny**.
  - Trusted devices are recognised by their Tailscale node identity or LAN hardware (MAC) address, so they stay trusted when their IP changes.
  - You get notifications when a session starts and ends.
- **Reconnect handling:**
  - A device that drops can come back within a grace period (10 minutes by default) without a new prompt.
  - Capture restarts automatically when monitors change, the stream fails or KWin restarts.
  - Clients detect dead links through TCP keepalives.
  - The systemd service restarts itself after a crash.
- **Start at login:** a systemd user service you can toggle from the settings page.
- **Settings page:** a local web UI at `127.0.0.1:5980` for status, sessions, display, security, trusted devices and startup.

## How it works

| Piece | Implementation |
|---|---|
| Screen capture | KWin's `zkde_screencast_unstable_v1` → PipeWire stream (shared memory, BGRx) |
| Input | KWin's `org_kde_kwin_fake_input` (absolute pointer, buttons, wheel, keysyms) |
| Protocol | RFB 3.3 / 3.7 / 3.8, VNC authentication, Raw + ZRLE, DesktopSize + ExtendedDesktopSize |
| Notifications | `org.freedesktop.Notifications` over D-Bus, with action buttons |

KWin only exposes the screencast and fake-input protocols to executables whose `.desktop` file lists them in `X-KDE-Wayland-Interfaces`. That's why no portal permission dialog appears, and why the binary must run from the path named in its `.desktop` entry. `remote-desk serve` and `remote-desk install` keep that entry up to date.

## Build and install

Requirements: Rust 1.88+, PipeWire, KDE Plasma 6 on Wayland, clang (for PipeWire bindings).

```sh
cargo build --release
./target/release/remote-desk install --autostart   # menu entry + start at login
./target/release/remote-desk settings              # set a password, then connect
```

For a system-wide install into `/usr/local`, run `sudo packaging/install-system.sh`. Each user then enables `remote-desk.service`.

### Updating

After pulling or changing the code, rebuild and restart the running service in one step:

```sh
packaging/update.sh            # per-user install
packaging/update.sh --system   # system-wide install (asks for sudo)
```

Connected clients are dropped during the restart. They can reconnect right away, and within the reconnect grace period they are not asked for approval again.

To do it by hand:

```sh
cargo build --release
systemctl --user restart remote-desk.service
journalctl --user -u remote-desk.service -f     # watch the log
```

To reinstall from scratch (settings and trusted devices are kept):

```sh
./target/release/remote-desk uninstall
./target/release/remote-desk install --autostart
```

### Commands

```
remote-desk serve                  run the server (what the service runs)
remote-desk settings               open the settings page
remote-desk set-password           set the VNC password from a terminal
remote-desk install [--autostart]  add the menu entry and the user service
remote-desk uninstall              remove them (settings are kept)
remote-desk snapshot out.png       save a screenshot (capture self-test)
```

Settings are stored in `~/.config/remote-desk/config.toml` (mode 0600).

## Security notes

- VNC authentication is DES-based and the password is limited to **8 characters**. The protocol is **not encrypted**.
- Keep the defaults: only localhost, LAN and Tailscale may connect, and new devices need your approval.
- Reach the computer from outside over **Tailscale** or an **SSH tunnel** (`ssh -L 5900:localhost:5900 you@host`), never by opening the port to the internet.
- Wrong passwords lock the source IP out with exponential back-off (30 s up to 1 h).
- The settings page listens only on `127.0.0.1`. It requires a per-run token plus a localhost `Host` header, and sends a strict Content-Security-Policy, so websites can't drive it.
- The server runs as **your user**, not root. Capture and input go through your Wayland session, and a network-facing parser should not run with root privileges.

## Starting before anyone logs in

Wayland screen capture needs a running desktop session. To make the computer reachable right after a reboot:
1. Enable SDDM automatic login for your user with the Plasma (Wayland) session. This needs admin rights.
2. Keep the lock screen enabled.

Remote Desk then starts with the session.

## Development

```sh
cargo test        # protocol, encoder (with an independent ZRLE decoder), network classification
RUST_LOG=remote_desk=debug cargo run --release -- serve
```

## Third-party protocol files

`protocols/zkde-screencast-unstable-v1.xml` and `protocols/fake-input.xml` come from KDE's [plasma-wayland-protocols](https://invent.kde.org/libraries/plasma-wayland-protocols) and are licensed LGPL-2.1-or-later.
