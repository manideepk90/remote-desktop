//! Per-user installation: the .desktop entry (which also grants KWin access to
//! the screencast and fake-input protocols) and the systemd user service.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

pub const DESKTOP_ID: &str = "dev.remotedesk.RemoteDesk";
const SYSTEM_UNIT: &str = "/usr/lib/systemd/user/remote-desk.service";
const SYSTEM_DESKTOP: &str = "/usr/share/applications/dev.remotedesk.RemoteDesk.desktop";

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
}

fn user_unit_path() -> PathBuf {
    crate::config::dir().parent().unwrap().join("systemd/user/remote-desk.service")
}

fn user_desktop_path() -> PathBuf {
    data_home().join(format!("applications/{DESKTOP_ID}.desktop"))
}

fn exe() -> Result<String> {
    let p = std::env::current_exe()?.canonicalize()?;
    let s = p.to_str().context("binary path is not UTF-8")?;
    anyhow::ensure!(!s.contains([' ', '"', '\'', '\\']), "move the binary to a path without spaces or quotes");
    Ok(s.to_string())
}

pub fn desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]
Type=Application
Name=Remote Desk
GenericName=Remote Desktop Server
Comment=Share this desktop with any VNC app
Exec={exe} settings
Icon=preferences-desktop-remote-desktop
Categories=System;Network;RemoteAccess;
Keywords=vnc;remote;desktop;share;screen;
StartupNotify=false
X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_kwin_fake_input
"
    )
}

pub fn service_unit(exe: &str) -> String {
    format!(
        "[Unit]
Description=Remote Desk VNC server
PartOf=graphical-session.target
After=graphical-session.target
StartLimitIntervalSec=0

[Service]
ExecStart={exe} serve
Restart=always
RestartSec=2

[Install]
WantedBy=graphical-session.target
"
    )
}

fn write_if_changed(path: &PathBuf, contents: &str) -> Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|c| c == contents) {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

/// Writes the user service unless a system-wide one is installed.
pub fn write_service_unit() -> Result<()> {
    if std::path::Path::new(SYSTEM_UNIT).exists() {
        return Ok(());
    }
    if write_if_changed(&user_unit_path(), &service_unit(&exe()?))? {
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    }
    Ok(())
}

pub fn write_desktop_entry() -> Result<()> {
    if std::path::Path::new(SYSTEM_DESKTOP).exists() {
        return Ok(());
    }
    if write_if_changed(&user_desktop_path(), &desktop_entry(&exe()?))? {
        // KWin reads the interface grants from the KDE service cache.
        let _ = Command::new("kbuildsycoca6").arg("--noincremental").output();
    }
    Ok(())
}

pub fn install(autostart: bool) -> Result<()> {
    write_desktop_entry()?;
    write_service_unit()?;
    if autostart {
        let ok = Command::new("systemctl").args(["--user", "enable", "--now", "remote-desk.service"]).status()?.success();
        anyhow::ensure!(ok, "could not enable the service");
    }
    println!("Installed for this user.");
    println!("  App menu entry: Remote Desk");
    println!("  Service: remote-desk.service ({})", if autostart { "starts at login" } else { "run `remote-desk install --autostart` to start at login" });
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = Command::new("systemctl").args(["--user", "disable", "--now", "remote-desk.service"]).status();
    for p in [user_unit_path(), user_desktop_path()] {
        if p.exists() {
            std::fs::remove_file(&p)?;
            println!("removed {}", p.display());
        }
    }
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    let _ = Command::new("kbuildsycoca6").output();
    println!("Settings and trusted devices are kept in {}", crate::config::dir().display());
    Ok(())
}
