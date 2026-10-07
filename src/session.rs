//! A private headless Plasma session for remote devices.
//!
//! It runs as the same user but has its own compositor (`kwin_wayland --virtual`),
//! D-Bus bus, desktop shell, pointer and keyboard, so remote use never moves the
//! mouse or steals focus on the desktop of the person at the computer.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Config files the session gets its own copy of, so its shell and compositor
/// never write over the layout of the main desktop. Everything else is shared.
const PRIVATE_CONFIG: &[&str] = &["plasma", "kwin", "kscreen", "kded", "ksmserver", "kactivitymanagerd", "powerdevil", "kglobalshortcuts"];

pub struct Session {
    child: Child,
    /// Wayland socket name inside `$XDG_RUNTIME_DIR`.
    pub socket: String,
}

impl Session {
    pub fn start(width: i32, height: i32) -> Result<Session> {
        let socket = format!("remote-desk-{}", std::process::id());
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?);
        let dir = crate::config::dir().join("session");
        let config_home = dir.join("config");
        prepare_config(&config_home)?;
        let log = std::fs::File::create(dir.join("session.log"))?;

        let child = Command::new("dbus-run-session")
            .args(["--", "kwin_wayland", "--virtual", "--no-lockscreen", "--xwayland", "--socket", &socket])
            .args(["--width", &width.to_string(), "--height", &height.to_string()])
            .args(["--exit-with-session", "plasmashell"])
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "KDE")
            .env("KDE_FULL_SESSION", "true")
            .env("KDE_SESSION_VERSION", "6")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()
            .context("cannot start kwin_wayland")?;
        let mut session = Session { child, socket };

        let path = runtime.join(&session.socket);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !path.exists() {
            if session.child.try_wait()?.is_some() {
                bail!("the session exited while starting; see {}", dir.join("session.log").display());
            }
            if Instant::now() > deadline {
                bail!("the session did not start within 15 s; see {}", dir.join("session.log").display());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log::info!("private session started on Wayland socket {}", session.socket);
        Ok(session)
    }
}

impl Drop for Session {
    /// Ends the whole process group: D-Bus, KWin, Xwayland, the shell and any apps.
    fn drop(&mut self) {
        let group = format!("-{}", self.child.id());
        let _ = Command::new("kill").args(["-TERM", "--", &group]).status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                log::info!("private session stopped");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new("kill").args(["-KILL", "--", &group]).status();
        let _ = self.child.wait();
        log::info!("private session killed");
    }
}

/// Builds the session's `XDG_CONFIG_HOME`: shell and compositor files are copied
/// once from the user's config, everything else is a symlink to it.
fn prepare_config(dest: &Path) -> Result<()> {
    let home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("cannot find the config directory")?;
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(&home)?.flatten() {
        let name = entry.file_name();
        let Some(n) = name.to_str() else { continue };
        let target = dest.join(&name);
        if n == "remote-desk" || target.symlink_metadata().is_ok() {
            continue;
        }
        if PRIVATE_CONFIG.iter().any(|p| n.starts_with(p)) {
            if entry.file_type()?.is_file() {
                std::fs::copy(entry.path(), &target)?;
            }
        } else {
            std::os::unix::fs::symlink(entry.path(), &target)?;
        }
    }
    Ok(())
}
