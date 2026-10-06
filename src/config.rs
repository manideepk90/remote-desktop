//! Persistent settings in `$XDG_CONFIG_HOME/remote-desk/config.toml` (mode 0600).

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// VNC port.
    pub port: u16,
    /// VNC password. The VNC protocol only uses the first 8 characters.
    pub password: String,
    pub allow_from: Networks,
    /// Ask the person at this computer before an unknown device connects.
    pub require_approval: bool,
    /// Remote devices can watch but not control.
    pub view_only: bool,
    pub display: Display,
    /// A device that drops can reconnect without a new prompt for this long.
    pub reconnect_grace_secs: u64,
    /// Settings page port (bound to 127.0.0.1 only).
    pub ui_port: u16,
    pub trusted: Vec<TrustedDevice>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Networks {
    /// 127.0.0.1 / ::1, i.e. SSH tunnels.
    pub localhost: bool,
    /// Private LAN ranges (10/8, 172.16/12, 192.168/16, link-local, IPv6 ULA).
    pub lan: bool,
    /// Tailscale (100.64.0.0/10, fd7a:115c:a1e0::/48).
    pub tailscale: bool,
    /// Anything else, including the public internet.
    pub internet: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Display {
    /// Monitor name such as "DP-4", or empty for all monitors.
    pub source: String,
    /// 1.0 = native resolution; 0.5 = half width and height.
    pub scale: f64,
    pub max_fps: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrustedDevice {
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub added: u64,
    #[serde(default)]
    pub last_seen: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 5900,
            password: String::new(),
            allow_from: Networks::default(),
            require_approval: true,
            view_only: false,
            display: Display::default(),
            reconnect_grace_secs: 600,
            ui_port: 5980,
            trusted: Vec::new(),
        }
    }
}

impl Default for Networks {
    fn default() -> Self {
        Networks { localhost: true, lan: true, tailscale: true, internet: false }
    }
}

impl Default for Display {
    fn default() -> Self {
        Display { source: String::new(), scale: 1.0, max_fps: 30 }
    }
}

impl Config {
    /// Clamps values that would break capture or the protocol.
    pub fn sanitize(&mut self) {
        self.display.scale = self.display.scale.clamp(0.25, 2.0);
        self.display.max_fps = self.display.max_fps.clamp(1, 120);
        if self.port == 0 {
            self.port = 5900;
        }
        if self.ui_port == 0 || self.ui_port == self.port {
            self.ui_port = 5980;
        }
    }
}

pub fn dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("remote-desk")
}

fn path() -> PathBuf {
    dir().join("config.toml")
}

pub fn load() -> Result<Config> {
    let p = path();
    let mut cfg = match std::fs::read_to_string(&p) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("invalid {}", p.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", p.display())),
    };
    cfg.sanitize();
    Ok(cfg)
}

/// Writes atomically with owner-only permissions (the file holds the password).
pub fn save(cfg: &Config) -> Result<()> {
    let d = dir();
    std::fs::create_dir_all(&d)?;
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
    let tmp = d.join("config.toml.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(toml::to_string_pretty(cfg)?.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path())?;
    Ok(())
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
