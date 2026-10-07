//! Shared application state: settings, capture, pairing, sessions and lockouts.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::{self, Capture};
use crate::config::{self, Config, DisplayMode, KeepMode, TrustedDevice, now};
use crate::pairing::{self, Device, Network, Pairing};
use crate::sessions::Sessions;

/// Failed password attempts allowed before an IP is locked out.
const FREE_ATTEMPTS: u32 = 5;

pub struct App {
    config: Mutex<Config>,
    pub capture: Arc<Capture>,
    pub pairing: Arc<Pairing>,
    pub sessions: Sessions,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    listen_error: Mutex<Option<String>>,
    hostname: String,
}

pub fn capture_settings(cfg: &Config) -> capture::Settings {
    let size = (cfg.display.virtual_width as i32, cfg.display.virtual_height as i32);
    capture::Settings {
        source: Some(cfg.display.source.clone()).filter(|s| !s.is_empty()),
        scale: cfg.display.scale,
        max_fps: cfg.display.max_fps,
        virtual_size: (cfg.display.mode == DisplayMode::Virtual).then_some(size),
        session_size: (cfg.display.mode == DisplayMode::Session).then_some(size),
        keep: match cfg.display.keep_after_disconnect {
            KeepMode::Stop => capture::Keep::Stop,
            KeepMode::Grace => capture::Keep::For(Duration::from_secs(cfg.display.keep_secs)),
            KeepMode::Always => capture::Keep::Always,
        },
    }
}

impl App {
    pub fn new(cfg: Config) -> Arc<App> {
        let hostname = std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "Linux".into());
        Arc::new(App {
            capture: Capture::start(capture_settings(&cfg)),
            config: Mutex::new(cfg),
            pairing: Pairing::new(),
            sessions: Sessions::default(),
            failures: Mutex::default(),
            listen_error: Mutex::default(),
            hostname,
        })
    }

    pub fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    /// Applies a change, saves it and pushes it to the running services.
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) -> Result<Config> {
        let mut cfg = self.config.lock().unwrap();
        let mut next = cfg.clone();
        f(&mut next);
        next.sanitize();
        config::save(&next)?;
        self.capture.update_settings(capture_settings(&next));
        // Devices that are no longer trusted lose their open sessions.
        for t in cfg.trusted.iter().filter(|t| !next.trusted.iter().any(|n| n.id == t.id)) {
            self.sessions.disconnect_device(&t.id);
            self.pairing.forget_recent(&t.id);
        }
        *cfg = next.clone();
        Ok(next)
    }

    pub fn trust(&self, device: &Device) {
        let res = self.update_config(|c| {
            if !c.trusted.iter().any(|t| t.id == device.id) {
                c.trusted.push(TrustedDevice { id: device.id.clone(), name: device.name.clone(), added: now(), last_seen: now() });
            }
        });
        if let Err(e) = res {
            log::warn!("could not save trusted device: {e:#}");
        }
    }

    pub fn touch_trusted(&self, id: &str) {
        let mut cfg = self.config.lock().unwrap();
        if let Some(t) = cfg.trusted.iter_mut().find(|t| t.id == id) {
            t.last_seen = now();
            let _ = config::save(&cfg);
        }
    }

    pub fn peer_allowed(&self, ip: IpAddr) -> bool {
        let n = self.config.lock().unwrap().allow_from.clone();
        match Network::of(ip) {
            Network::Localhost => n.localhost,
            Network::Lan => n.lan,
            Network::Tailscale => n.tailscale,
            Network::Internet => n.internet,
        }
    }

    pub fn identify(&self, ip: IpAddr) -> Device {
        let mut d = pairing::identify(ip);
        // Prefer the name the user gave a trusted device.
        if let Some(t) = self.config.lock().unwrap().trusted.iter().find(|t| t.id == d.id) {
            d.name = t.name.clone();
        }
        d
    }

    /// Exponential lockout after repeated wrong passwords: 30s, 60s, 120s ... up to 1h.
    pub fn lockout_remaining(&self, ip: IpAddr) -> Option<Duration> {
        let f = self.failures.lock().unwrap();
        let &(count, last) = f.get(&ip)?;
        if count < FREE_ATTEMPTS {
            return None;
        }
        let wait = Duration::from_secs(30 << (count - FREE_ATTEMPTS).min(7)).min(Duration::from_secs(3600));
        wait.checked_sub(last.elapsed())
    }

    pub fn auth_failed(&self, ip: IpAddr) {
        let mut f = self.failures.lock().unwrap();
        let e = f.entry(ip).or_insert((0, Instant::now()));
        e.0 += 1;
        e.1 = Instant::now();
        log::warn!("wrong VNC password from {ip} ({} attempts)", e.0);
    }

    pub fn auth_succeeded(&self, ip: IpAddr) {
        self.failures.lock().unwrap().remove(&ip);
    }

    pub fn input_allowed(&self, session: u64) -> bool {
        !self.config.lock().unwrap().view_only && !self.sessions.is_view_only(session)
    }

    pub fn desktop_name(&self) -> String {
        format!("{} (Remote Desk)", self.hostname)
    }

    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    pub fn set_listen_error(&self, e: Option<String>) {
        if let Some(e) = &e {
            log::warn!("{e}");
        }
        *self.listen_error.lock().unwrap() = e;
    }

    pub fn listen_error(&self) -> Option<String> {
        self.listen_error.lock().unwrap().clone()
    }
}
