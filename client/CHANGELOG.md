# Changelog

## [0.1.0] - 2026-10-13

The first release of Remote Desk Viewer, a VNC viewer for Windows (x64).

### Highlights

- **Connect by IP address or host name** (an optional `:port` works too), with an optional **username** for servers that log in with an account (macOS Screen Sharing, TigerVNC Plain).
- **Remember passwords securely** in **Windows Credential Manager**. A password is saved only after the server accepts it, and never written to a file.
- **Full keyboard and mouse control:**
  - Modifiers, function and navigation keys, typing in your local keyboard layout.
  - All mouse buttons, dragging, and vertical and horizontal scrolling.
  - Ctrl+Alt+Del button and a view-only mode.
- **Saved connections**, sorted by last use. Double-click one to connect.
- **Clipboard sync:** remote copies arrive on your clipboard, and Ctrl+V sends yours to the remote computer before pasting.
- **Fit-to-window or 1:1 scaling**, full screen (Ctrl+Alt+Enter), and live remote resolution changes.
- Works with the Remote Desk server in this repository, including its "waiting for approval" step, and with standard VNC servers such as TigerVNC. Also supports the account login used by macOS Screen Sharing.

### Downloads

- `remote-desk-viewer-0.1.0-windows-x64.exe`: a portable app, just run it.
- `remote-desk-viewer-0.1.0-windows-x64.zip`: the same app with its README and license.
- `SHA256SUMS.txt`: checksums for both.

The app isn't code-signed yet, so Windows SmartScreen may warn the first time you run it. Choose **More info → Run anyway**.

### Known limitations

- VNC traffic isn't encrypted, and TLS logins (VeNCrypt TLS/X509) aren't supported yet. Outside a trusted network, use Tailscale or an SSH tunnel.
- On keyboard layouts that type characters with AltGr, Windows reports AltGr as Ctrl+Alt, so those characters arrive at the remote computer as Ctrl+Alt shortcuts.
