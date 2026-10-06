//! Settings page and JSON API, served on 127.0.0.1 only.
//!
//! Every API call must carry the per-run token (from `ui-token`, handed to the
//! browser in the URL fragment by `remote-desk settings`) and a localhost Host
//! header, so web pages and DNS-rebinding attacks cannot drive the API.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::app::App;
use crate::config::{self, Display, Networks};
use crate::pairing::Decision;

const INDEX: &str = include_str!("../ui/index.html");
const SCRIPT: &str = include_str!("../ui/app.js");
const STYLE: &str = include_str!("../ui/style.css");
pub const SERVICE: &str = "remote-desk.service";

pub fn token_path() -> std::path::PathBuf {
    config::dir().join("ui-token")
}

pub fn start(app: Arc<App>) -> Result<()> {
    let port = app.config().ui_port;
    let server = Server::http(("127.0.0.1", port)).map_err(|e| anyhow::anyhow!("settings page on port {port}: {e}"))?;
    let token = hex(&rand::random::<[u8; 24]>());
    std::fs::create_dir_all(config::dir())?;
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(token_path())?;
        f.write_all(token.as_bytes())?;
    }
    log::info!("settings page on http://127.0.0.1:{port} (open it with `remote-desk settings`)");
    std::thread::Builder::new().name("ui".into()).spawn(move || {
        for req in server.incoming_requests() {
            handle(&app, &token, port, req);
        }
    })?;
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn header<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn respond(req: Request, status: u16, ctype: &str, body: impl Into<Vec<u8>>) {
    let mut r = Response::from_data(body.into()).with_status_code(status);
    for (k, v) in [
        ("Content-Type", ctype),
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "DENY"),
        ("Referrer-Policy", "no-referrer"),
        ("Content-Security-Policy", "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"),
    ] {
        r.add_header(Header::from_bytes(k, v).unwrap());
    }
    let _ = req.respond(r);
}

fn json_reply(req: Request, status: u16, v: Value) {
    respond(req, status, "application/json", v.to_string());
}

fn handle(app: &Arc<App>, token: &str, port: u16, mut req: Request) {
    let host_ok = header(&req, "Host").is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
    if !host_ok {
        return respond(req, 421, "text/plain", "wrong host");
    }
    let url = req.url().split('?').next().unwrap_or("/").to_string();
    let method = req.method().clone();
    match (&method, url.as_str()) {
        (Method::Get, "/") => return respond(req, 200, "text/html; charset=utf-8", INDEX),
        (Method::Get, "/app.js") => return respond(req, 200, "text/javascript; charset=utf-8", SCRIPT),
        (Method::Get, "/style.css") => return respond(req, 200, "text/css; charset=utf-8", STYLE),
        _ => {}
    }
    if !url.starts_with("/api/") {
        return respond(req, 404, "text/plain", "not found");
    }
    if !header(&req, "X-Token").is_some_and(|t| ct_eq(t, token)) {
        return json_reply(req, 401, json!({"error": "Open this page with `remote-desk settings` or the Remote Desk app icon."}));
    }
    let mut body = String::new();
    if req.as_reader().take(1 << 20).read_to_string(&mut body).is_err() {
        return json_reply(req, 400, json!({"error": "bad body"}));
    }
    let parts: Vec<&str> = url.trim_start_matches("/api/").split('/').collect();
    let result = route(app, &method, &parts, &body);
    match result {
        Ok(v) => json_reply(req, 200, v),
        Err(e) => json_reply(req, 400, json!({"error": format!("{e:#}")})),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsPatch {
    port: Option<u16>,
    allow_from: Option<Networks>,
    require_approval: Option<bool>,
    view_only: Option<bool>,
    display: Option<Display>,
    reconnect_grace_secs: Option<u64>,
}

fn route(app: &Arc<App>, method: &Method, parts: &[&str], body: &str) -> Result<Value> {
    let parse = || -> Result<Value> { serde_json::from_str(body).context("invalid JSON") };
    let id = |i: usize| -> Result<u64> { parts.get(i).and_then(|s| s.parse().ok()).context("bad id") };
    match (method, parts) {
        (Method::Get, ["state"]) => Ok(state(app)),
        (Method::Put, ["settings"]) => {
            let p: SettingsPatch = serde_json::from_str(body).context("invalid settings")?;
            if let Some(port) = p.port
                && port < 1024
            {
                anyhow::bail!("choose a port from 1024 to 65535");
            }
            app.update_config(|c| {
                if let Some(v) = p.port {
                    c.port = v;
                }
                if let Some(v) = p.allow_from {
                    c.allow_from = v;
                }
                if let Some(v) = p.require_approval {
                    c.require_approval = v;
                }
                if let Some(v) = p.view_only {
                    c.view_only = v;
                }
                if let Some(v) = p.display {
                    c.display = v;
                }
                if let Some(v) = p.reconnect_grace_secs {
                    c.reconnect_grace_secs = v.min(86_400);
                }
            })?;
            Ok(state(app))
        }
        (Method::Post, ["password"]) => {
            let pw = parse()?["password"].as_str().unwrap_or_default().to_string();
            crate::validate_password(&pw)?;
            app.update_config(|c| c.password = pw)?;
            Ok(state(app))
        }
        (Method::Post, ["pending", _]) => {
            let d: Decision = serde_json::from_value(parse()?["decision"].clone()).context("decision must be once, always or deny")?;
            if !app.pairing.decide(id(1)?, d) {
                anyhow::bail!("that request is no longer waiting");
            }
            Ok(state(app))
        }
        (Method::Post, ["sessions", _, "disconnect"]) => {
            app.sessions.disconnect(id(1)?);
            Ok(state(app))
        }
        (Method::Post, ["sessions", _, "view_only"]) => {
            let v = parse()?["view_only"].as_bool().unwrap_or(true);
            app.sessions.set_view_only(id(1)?, v);
            Ok(state(app))
        }
        (Method::Post, ["trusted", "remove"]) => {
            let tid = parse()?["id"].as_str().unwrap_or_default().to_string();
            app.update_config(|c| c.trusted.retain(|t| t.id != tid))?;
            Ok(state(app))
        }
        (Method::Post, ["trusted", "rename"]) => {
            let v = parse()?;
            let (tid, name) = (v["id"].as_str().unwrap_or_default(), v["name"].as_str().unwrap_or_default().trim());
            if name.is_empty() || name.len() > 64 {
                anyhow::bail!("names must be 1-64 characters");
            }
            app.update_config(|c| {
                if let Some(t) = c.trusted.iter_mut().find(|t| t.id == tid) {
                    t.name = name.to_string();
                }
            })?;
            Ok(state(app))
        }
        (Method::Post, ["autostart"]) => {
            let enable = parse()?["enabled"].as_bool().unwrap_or(false);
            set_autostart(enable)?;
            Ok(state(app))
        }
        _ => anyhow::bail!("unknown request"),
    }
}

fn state(app: &App) -> Value {
    let cfg = app.config();
    let frame = app.capture.store.latest();
    let mut cfg_json = serde_json::to_value(&cfg).unwrap_or_default();
    if let Some(o) = cfg_json.as_object_mut() {
        o.remove("password");
    }
    json!({
        "hostname": app.hostname(),
        "user": std::env::var("USER").unwrap_or_default(),
        "version": env!("CARGO_PKG_VERSION"),
        "capture": app.capture.status(),
        "listen_error": app.listen_error(),
        "frame": frame.map(|f| json!({"width": f.width, "height": f.height})),
        "password_set": !cfg.password.is_empty(),
        "config": cfg_json,
        "outputs": app.capture.outputs(),
        "addresses": addresses(),
        "sessions": app.sessions.list(),
        "pending": app.pairing.pending(),
        "autostart": autostart_state(),
    })
}

/// LAN and Tailscale addresses people can connect to (skips container bridges).
fn addresses() -> Vec<Value> {
    let Ok(out) = Command::new("ip").args(["-j", "-4", "addr", "show", "up"]).output() else { return vec![] };
    let Ok(ifs) = serde_json::from_slice::<Vec<Value>>(&out.stdout) else { return vec![] };
    let mut list = Vec::new();
    for i in ifs {
        let name = i["ifname"].as_str().unwrap_or_default();
        if name == "lo" || ["docker", "br-", "veth", "virbr", "vmnet"].iter().any(|p| name.starts_with(p)) {
            continue;
        }
        for a in i["addr_info"].as_array().into_iter().flatten() {
            let Some(ip) = a["local"].as_str() else { continue };
            let kind = if name.starts_with("tailscale") { "Tailscale" } else { "Local network" };
            list.push(json!({"interface": name, "ip": ip, "kind": kind}));
        }
    }
    list
}

fn systemctl(args: &[&str]) -> Option<String> {
    let out = Command::new("systemctl").arg("--user").args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn autostart_state() -> Value {
    let enabled = systemctl(&["is-enabled", SERVICE]).unwrap_or_default();
    let active = systemctl(&["is-active", SERVICE]).unwrap_or_default();
    json!({
        "installed": !matches!(enabled.as_str(), "" | "not-found"),
        "enabled": enabled == "enabled",
        "running_as_service": active == "active" && std::env::var_os("INVOCATION_ID").is_some(),
    })
}

fn set_autostart(enable: bool) -> Result<()> {
    if enable {
        crate::install::write_service_unit()?;
    }
    let status = Command::new("systemctl")
        .args(["--user", if enable { "enable" } else { "disable" }, SERVICE])
        .status()
        .context("systemctl not available")?;
    anyhow::ensure!(status.success(), "systemctl failed to {} the service", if enable { "enable" } else { "disable" });
    Ok(())
}
