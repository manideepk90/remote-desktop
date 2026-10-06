//! Device identity, connection approval and desktop notifications.
//!
//! After a client passes the VNC password, it still needs approval unless it is
//! a trusted device or is reconnecting within the grace period. Approval comes
//! from a desktop notification (Allow once / Always allow / Deny) or the
//! settings page.

use std::collections::HashMap;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::app::App;
use crate::config::now;

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize)]
pub struct Device {
    /// Stable identity: Tailscale node ID, LAN MAC address, or IP.
    pub id: String,
    pub name: String,
    pub ip: IpAddr,
    pub network: Network,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub enum Network {
    Localhost,
    Lan,
    Tailscale,
    Internet,
}

impl Network {
    pub fn of(ip: IpAddr) -> Network {
        match ip {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                if v4.is_loopback() {
                    Network::Localhost
                } else if o[0] == 100 && (64..128).contains(&o[1]) {
                    Network::Tailscale
                } else if v4.is_private() || v4.is_link_local() {
                    Network::Lan
                } else {
                    Network::Internet
                }
            }
            IpAddr::V6(v6) => {
                let s = v6.segments();
                if v6.is_loopback() {
                    Network::Localhost
                } else if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                    Network::Tailscale
                } else if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 {
                    Network::Lan
                } else {
                    Network::Internet
                }
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Network::Localhost => "this computer (SSH tunnel)",
            Network::Lan => "your local network",
            Network::Tailscale => "Tailscale",
            Network::Internet => "the internet",
        }
    }
}

/// Runs a short helper command, giving up after `timeout`.
fn run(cmd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(cmd).args(args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    Some(out)
}

pub fn identify(ip: IpAddr) -> Device {
    let network = Network::of(ip);
    let fallback = Device { id: format!("ip:{ip}"), name: ip.to_string(), ip, network };
    match network {
        Network::Localhost => Device { id: "local".into(), name: "SSH tunnel on this computer".into(), ip, network },
        Network::Tailscale => tailscale_device(ip).unwrap_or(fallback),
        Network::Lan => {
            let name = reverse_dns(ip).unwrap_or_else(|| ip.to_string());
            match mac_address(ip) {
                Some(mac) => Device { id: format!("mac:{mac}"), name, ip, network },
                None => Device { name, ..fallback },
            }
        }
        Network::Internet => fallback,
    }
}

fn tailscale_device(ip: IpAddr) -> Option<Device> {
    let out = run("tailscale", &["whois", "--json", &ip.to_string()], Duration::from_secs(3))?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let node = &v["Node"];
    let id = node["StableID"].as_str().or(node["ID"].as_str().filter(|s| !s.is_empty()))?;
    let host = node["ComputedName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| node["Name"].as_str().and_then(|n| n.split('.').next()))
        .unwrap_or("Tailscale device");
    let os = node["Hostinfo"]["OS"].as_str().filter(|s| !s.is_empty());
    let name = match os {
        Some(os) => format!("{host} ({os})"),
        None => host.to_string(),
    };
    Some(Device { id: format!("ts:{id}"), name, ip, network: Network::Tailscale })
}

fn mac_address(ip: IpAddr) -> Option<String> {
    let arp = std::fs::read_to_string("/proc/net/arp").ok()?;
    let ip = ip.to_string();
    arp.lines().skip(1).find_map(|l| {
        let cols: Vec<&str> = l.split_whitespace().collect();
        (cols.first() == Some(&ip.as_str()) && cols.get(3).is_some_and(|m| *m != "00:00:00:00:00:00"))
            .then(|| cols[3].to_lowercase())
    })
}

fn reverse_dns(ip: IpAddr) -> Option<String> {
    let out = run("getent", &["hosts", &ip.to_string()], Duration::from_secs(1))?;
    let name = out.split_whitespace().nth(1)?;
    Some(name.trim_end_matches('.').to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Once,
    Always,
    Deny,
}

struct Pending {
    id: u64,
    device: Device,
    since: u64,
    notification: Option<u32>,
    decision: Option<Decision>,
}

#[derive(Serialize)]
pub struct PendingInfo {
    pub id: u64,
    pub device: Device,
    pub since: u64,
}

pub struct Pairing {
    pending: Mutex<Vec<Pending>>,
    cv: Condvar,
    /// Device id -> when it last disconnected.
    recent: Mutex<HashMap<String, Instant>>,
    next_id: AtomicU64,
    notifier: Option<Notifier>,
}

impl Pairing {
    pub fn new() -> Arc<Pairing> {
        let notifier = Notifier::connect().map_err(|e| log::warn!("desktop notifications unavailable: {e}")).ok();
        let p = Arc::new(Pairing {
            pending: Mutex::default(),
            cv: Condvar::new(),
            recent: Mutex::default(),
            next_id: AtomicU64::new(1),
            notifier,
        });
        if let Some(n) = &p.notifier {
            n.listen(Arc::downgrade(&p));
        }
        p
    }

    /// Blocks until the device is allowed or refused. `Err` carries the reason shown to the client.
    pub fn authorize(&self, app: &App, device: &Device) -> Result<(), String> {
        let cfg = app.config();
        if cfg.trusted.iter().any(|t| t.id == device.id) {
            app.touch_trusted(&device.id);
            return Ok(());
        }
        if !cfg.require_approval {
            return Ok(());
        }
        let grace = Duration::from_secs(cfg.reconnect_grace_secs);
        let recently = self.recent.lock().unwrap().get(&device.id).is_some_and(|t| t.elapsed() < grace);
        if recently || app.sessions.has_device(&device.id) {
            log::info!("{} reconnected within the grace period", device.name);
            return Ok(());
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let notification = self.notifier.as_ref().and_then(|n| {
            n.send(
                "Allow remote access?",
                &format!(
                    "<b>{}</b> ({}) wants to view and control this computer over {}.",
                    escape(&device.name),
                    device.ip,
                    device.network.label()
                ),
                &["once", "Allow once", "always", "Always allow", "deny", "Deny"],
                2,
                0,
            )
        });
        self.pending.lock().unwrap().push(Pending { id, device: device.clone(), since: now(), notification, decision: None });
        log::info!("waiting for approval of {} ({})", device.name, device.ip);

        let pending = self.pending.lock().unwrap();
        let (mut pending, _) = self
            .cv
            .wait_timeout_while(pending, APPROVAL_TIMEOUT, |p| {
                p.iter().find(|x| x.id == id).is_some_and(|x| x.decision.is_none())
            })
            .unwrap();
        let entry = pending.iter().position(|x| x.id == id).map(|i| pending.remove(i));
        drop(pending);
        let decision = entry.as_ref().and_then(|e| e.decision);
        if let (Some(n), Some(nid)) = (&self.notifier, entry.and_then(|e| e.notification)) {
            n.close(nid);
        }
        match decision {
            Some(Decision::Once) => Ok(()),
            Some(Decision::Always) => {
                app.trust(device);
                Ok(())
            }
            Some(Decision::Deny) => Err("The connection was declined".into()),
            None => Err("Nobody approved the connection in time".into()),
        }
    }

    pub fn decide(&self, id: u64, decision: Decision) -> bool {
        let mut pending = self.pending.lock().unwrap();
        let Some(p) = pending.iter_mut().find(|p| p.id == id && p.decision.is_none()) else { return false };
        p.decision = Some(decision);
        self.cv.notify_all();
        true
    }

    fn decide_by_notification(&self, nid: u32, decision: Option<Decision>) {
        let id = self.pending.lock().unwrap().iter().find(|p| p.notification == Some(nid)).map(|p| p.id);
        if let Some(id) = id {
            // Dismissing the prompt without choosing counts as "deny".
            self.decide(id, decision.unwrap_or(Decision::Deny));
        }
    }

    pub fn pending(&self) -> Vec<PendingInfo> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.decision.is_none())
            .map(|p| PendingInfo { id: p.id, device: p.device.clone(), since: p.since })
            .collect()
    }

    pub fn forget_recent(&self, device_id: &str) {
        self.recent.lock().unwrap().remove(device_id);
    }

    pub fn notify_connected(&self, device: &Device) {
        if let Some(n) = &self.notifier {
            n.send(
                "Remote session started",
                &format!("<b>{}</b> is now connected to this computer.", escape(&device.name)),
                &[],
                1,
                6000,
            );
        }
    }

    pub fn notify_disconnected(&self, device: &Device) {
        self.recent.lock().unwrap().insert(device.id.clone(), Instant::now());
        if let Some(n) = &self.notifier {
            n.send("Remote session ended", &format!("<b>{}</b> disconnected.", escape(&device.name)), &[], 0, 4000);
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

struct Notifier {
    proxy: NotificationsProxyBlocking<'static>,
}

impl Notifier {
    fn connect() -> zbus::Result<Notifier> {
        let conn = zbus::blocking::Connection::session()?;
        Ok(Notifier { proxy: NotificationsProxyBlocking::new(&conn)? })
    }

    /// `urgency`: 0 low, 1 normal, 2 critical (stays until answered). `timeout_ms` 0 = never.
    fn send(&self, summary: &str, body: &str, actions: &[&str], urgency: u8, timeout_ms: i32) -> Option<u32> {
        let mut hints = HashMap::new();
        hints.insert("urgency", zbus::zvariant::Value::U8(urgency));
        hints.insert("desktop-entry", zbus::zvariant::Value::from("dev.remotedesk.RemoteDesk"));
        self.proxy
            .notify("Remote Desk", 0, "preferences-desktop-remote-desktop", summary, body, actions, hints, timeout_ms)
            .map_err(|e| log::warn!("notification failed: {e}"))
            .ok()
    }

    fn close(&self, id: u32) {
        let _ = self.proxy.close_notification(id);
    }

    fn listen(&self, pairing: std::sync::Weak<Pairing>) {
        let actions = self.proxy.clone();
        let p = pairing.clone();
        std::thread::Builder::new()
            .name("notify-actions".into())
            .spawn(move || {
                let Ok(signals) = actions.receive_action_invoked() else { return };
                for sig in signals {
                    let (Ok(args), Some(p)) = (sig.args(), p.upgrade()) else { continue };
                    let decision = match args.action_key.as_str() {
                        "once" => Decision::Once,
                        "always" => Decision::Always,
                        _ => Decision::Deny,
                    };
                    p.decide_by_notification(args.id, Some(decision));
                }
            })
            .ok();
        let closed = self.proxy.clone();
        std::thread::Builder::new()
            .name("notify-closed".into())
            .spawn(move || {
                let Ok(signals) = closed.receive_notification_closed() else { return };
                for sig in signals {
                    let (Ok(args), Some(p)) = (sig.args(), pairing.upgrade()) else { continue };
                    // reason 2 = dismissed by the user; expiry (1) and our own close (3) are not answers.
                    if args.reason == 2 {
                        // Give a racing ActionInvoked signal a moment to land first.
                        std::thread::sleep(Duration::from_millis(300));
                        p.decide_by_notification(args.id, None);
                    }
                }
            })
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_networks() {
        assert_eq!(Network::of("100.70.234.41".parse().unwrap()), Network::Tailscale);
        assert_eq!(Network::of("100.128.0.1".parse().unwrap()), Network::Internet);
        assert_eq!(Network::of("192.168.31.5".parse().unwrap()), Network::Lan);
        assert_eq!(Network::of("127.0.0.1".parse().unwrap()), Network::Localhost);
        assert_eq!(Network::of("fd7a:115c:a1e0::1".parse().unwrap()), Network::Tailscale);
        assert_eq!(Network::of("8.8.8.8".parse().unwrap()), Network::Internet);
    }
}
