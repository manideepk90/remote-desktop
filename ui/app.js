"use strict";

// The launcher passes the API token in the URL fragment; keep it for reloads.
const token = (() => {
  const m = location.hash.match(/(?:^#|&)t=([0-9a-f]+)/);
  if (m) {
    try { sessionStorage.setItem("rd-token", m[1]); } catch { /* storage unavailable */ }
    return m[1];
  }
  try { return sessionStorage.getItem("rd-token") || ""; } catch { return ""; }
})();

const $ = (id) => document.getElementById(id);
let state = null;
let renamingId = null;
let lastLayoutKey = "";

async function api(method, path, body) {
  const headers = { "X-Token": token };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const res = await fetch("/api/" + path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  const data = await res.json().catch(() => ({ error: "Unexpected response from Remote Desk" }));
  if (!res.ok) throw new Error(data.error || res.statusText);
  return data;
}

async function act(method, path, body, message) {
  try {
    state = await api(method, path, body);
    render();
    if (message) toast(message);
    return true;
  } catch (e) {
    toast(e.message, true);
    return false;
  }
}

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === false || v == null) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else if (k === "style") el.style.cssText = v; // CSSOM is allowed by the CSP; style attributes are not
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

let toastTimer;
function toast(text, error = false) {
  const t = $("toast");
  t.textContent = text;
  t.className = "toast show" + (error ? " error" : "");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.className = "toast" + (error ? " error" : "")), error ? 5000 : 2200);
}

// ---------- formatting ----------
function ago(unix) {
  if (!unix) return "never";
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return new Date(unix * 1000).toLocaleDateString();
}
function duration(unix) {
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unix));
  const hh = Math.floor(s / 3600), mm = Math.floor((s % 3600) / 60), ss = s % 60;
  return hh ? `${hh}h ${mm}m` : mm ? `${mm}m ${ss}s` : `${ss}s`;
}
function bytes(n) {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}
const networkLabel = { Localhost: "SSH tunnel", Lan: "Local network", Tailscale: "Tailscale", Internet: "Internet" };
function idKind(id) {
  if (id.startsWith("ts:")) return "Tailscale device";
  if (id.startsWith("mac:")) return "Network adapter " + id.slice(4);
  if (id === "local") return "This computer";
  return "IP " + id.replace(/^ip:/, "");
}

// ---------- tabs ----------
function showTab(name) {
  for (const b of document.querySelectorAll(".tabs button")) b.setAttribute("aria-selected", String(b.dataset.tab === name));
  for (const p of document.querySelectorAll("[data-panel]")) p.hidden = p.dataset.panel !== name;
  try { localStorage.setItem("rd-tab", name); } catch { /* ignore */ }
  if (name === "display") { lastLayoutKey = ""; renderDisplay(); }
}
for (const b of document.querySelectorAll(".tabs button")) b.addEventListener("click", () => showTab(b.dataset.tab));

// ---------- rendering ----------
function render() {
  if (!state) return;
  const cfg = state.config;
  $("host").textContent = `${state.hostname} · version ${state.version}`;
  renderStatus();
  renderPending();
  renderAlerts();
  renderAddresses();
  renderSessions();
  renderDisplay();
  renderSecurity(cfg);
  renderTrusted(cfg);
  renderStartup();
}

function renderStatus() {
  const pill = $("status-pill");
  const n = state.sessions.length;
  let cls = "ok", text = "Ready";
  if (state.listen_error) { cls = "bad"; text = "Not reachable"; }
  else if (!state.password_set) { cls = "warn"; text = "Needs a password"; }
  else if (state.pending.length) { cls = "live"; text = "Waiting for you"; }
  else if (n) { cls = "live"; text = n === 1 ? "1 device connected" : `${n} devices connected`; }
  else if (!state.capture.startsWith("streaming")) { cls = "warn"; text = "Starting…"; }
  pill.className = "pill " + cls;
  pill.textContent = text;
  $("capture-status").textContent = state.frame ? `Sharing ${state.frame.width} × ${state.frame.height}` : state.capture;
}

function renderPending() {
  const box = $("pending");
  box.replaceChildren(...state.pending.map((p) =>
    h("div", { class: "banner ask" },
      h("div", {},
        h("b", {}, `${p.device.name} wants to connect`),
        h("div", { class: "small muted" }, `${p.device.ip} via ${networkLabel[p.device.network]} · asked ${ago(p.since)}`)),
      h("div", { class: "actions" },
        h("button", { class: "primary", onclick: () => act("POST", `pending/${p.id}`, { decision: "once" }, "Allowed for this session") }, "Allow once"),
        h("button", { class: "ghost", onclick: () => act("POST", `pending/${p.id}`, { decision: "always" }, "Device trusted") }, "Always allow"),
        h("button", { class: "danger", onclick: () => act("POST", `pending/${p.id}`, { decision: "deny" }, "Denied") }, "Deny")))));
}

function renderAlerts() {
  const items = [];
  if (state.listen_error) items.push(h("div", { class: "banner warn" }, h("span", {}, state.listen_error)));
  if (!state.password_set) {
    items.push(h("div", { class: "banner warn" },
      h("span", {}, h("b", {}, "Set a VNC password to start accepting connections.")),
      h("button", { class: "primary", onclick: () => { showTab("security"); $("password").focus(); } }, "Set password")));
  }
  if (!state.frame && !state.capture.startsWith("streaming")) {
    items.push(h("div", { class: "banner warn" }, h("span", {}, "Screen capture: " + state.capture)));
  }
  $("alerts").replaceChildren(...items);
}

function renderAddresses() {
  const port = state.config.port;
  const list = state.addresses.map((a) => {
    const addr = `${a.ip}:${port}`;
    return h("li", {},
      h("div", { class: "who" }, h("span", { class: "addr" }, addr), h("span", { class: "small muted" }, a.kind)),
      h("button", { class: "ghost", onclick: () => copy(addr) }, "Copy"));
  });
  $("addresses").replaceChildren(...(list.length ? list : [h("li", { class: "empty" }, "No network connection found.")]));
  const ts = state.addresses.find((a) => a.kind === "Tailscale") || state.addresses[0];
  $("ssh-hint").textContent = `ssh -L 5900:localhost:${port} ${state.user || "you"}@${ts ? ts.ip : "this-computer"}`;
}

async function copy(text) {
  try { await navigator.clipboard.writeText(text); toast("Copied " + text); }
  catch { toast("Copy failed: select the address and copy it manually", true); }
}

function renderSessions() {
  const items = state.sessions.map((s) => {
    const control = !state.config.view_only && !s.view_only;
    return h("li", {},
      h("div", { class: "who" },
        h("b", {}, s.device.name),
        h("span", { class: "meta" }, `${s.device.ip} · ${networkLabel[s.device.network]} · connected ${duration(s.connected_at)} · ${bytes(s.bytes_sent)} sent`)),
      h("div", { class: "actions" },
        h("span", { class: "tag" }, control ? "Can control" : "View only"),
        state.config.view_only ? null : h("button", {
          class: "ghost",
          onclick: () => act("POST", `sessions/${s.id}/view_only`, { view_only: !s.view_only }, s.view_only ? "Control enabled" : "Switched to view only"),
        }, s.view_only ? "Allow control" : "View only"),
        h("button", { class: "danger", onclick: () => act("POST", `sessions/${s.id}/disconnect`, undefined, "Disconnected") }, "Disconnect")));
  });
  $("sessions").replaceChildren(...(items.length ? items : [h("li", { class: "empty" }, "Nobody is connected.")]));
}

function bbox(outs) {
  const x0 = Math.min(...outs.map((o) => o.x)), y0 = Math.min(...outs.map((o) => o.y));
  const x1 = Math.max(...outs.map((o) => o.x + o.width)), y1 = Math.max(...outs.map((o) => o.y + o.height));
  return { x: x0, y: y0, width: x1 - x0, height: y1 - y0 };
}

function renderDisplay() {
  if (!state) return;
  const d = state.config.display;
  const outs = state.outputs;
  const selected = outs.find((o) => o.name === d.source);
  const key = JSON.stringify([outs, d.source]);
  const box = $("monitors");
  if (key !== lastLayoutKey && box.offsetParent !== null) {
    lastLayoutKey = key;
    const all = h("button", {
      class: "segmented-like ghost all", "aria-pressed": String(!selected),
      onclick: () => saveDisplay({ source: "" }),
    }, selected ? "Share all monitors" : "✓ Sharing all monitors");
    const layout = h("div", { class: "layout" });
    if (outs.length) {
      const b = bbox(outs);
      const width = box.clientWidth || 600, height = 170;
      const k = Math.min(width / b.width, height / b.height);
      const offX = (width - b.width * k) / 2;
      for (const o of outs) {
        const sel = selected ? selected.name === o.name : true;
        layout.append(h("button", {
          class: "monitor" + (sel ? " selected" : ""),
          style: `left:${offX + (o.x - b.x) * k + 3}px;top:${(o.y - b.y) * k + 3}px;width:${o.width * k - 6}px;height:${o.height * k - 6}px`,
          title: o.description,
          onclick: () => saveDisplay({ source: o.name }),
        }, h("b", {}, o.name), `${o.pixel_width} × ${o.pixel_height}`, h("span", { class: "small" }, o.description)));
      }
    } else {
      layout.append(h("p", { class: "muted" }, "No monitors detected yet."));
    }
    box.replaceChildren(layout, all);
  }

  segmented($("scale"), [[1, "Native"], [0.75, "75%"], [0.5, "50%"], [0.33, "33%"]], d.scale, (v) => saveDisplay({ scale: v }));
  segmented($("fps"), [[15, "15 fps"], [30, "30 fps"], [60, "60 fps"]], d.max_fps, (v) => saveDisplay({ max_fps: v }));
  const base = selected || (outs.length ? bbox(outs) : null);
  $("scale-result").textContent = base
    ? `${base.width} × ${base.height} shared as ${Math.round(base.width * d.scale)} × ${Math.round(base.height * d.scale)}`
    : "";
}

function segmented(el, options, current, onPick) {
  el.replaceChildren(...options.map(([v, label]) =>
    h("button", { "aria-pressed": String(Math.abs(v - current) < 0.01), onclick: () => onPick(v) }, label)));
}

function saveDisplay(patch) {
  return act("PUT", "settings", { display: { ...state.config.display, ...patch } }, "Display updated");
}

function renderSecurity(cfg) {
  for (const el of document.querySelectorAll("[data-setting]")) el.checked = !!cfg[el.dataset.setting];
  for (const el of document.querySelectorAll("[data-net]")) el.checked = !!cfg.allow_from[el.dataset.net];
  const grace = $("grace");
  if (document.activeElement !== grace) {
    const v = String(cfg.reconnect_grace_secs);
    if (![...grace.options].some((o) => o.value === v)) grace.append(h("option", { value: v }, `${v} seconds`));
    grace.value = v;
  }
  const port = $("port");
  if (document.activeElement !== port) port.value = cfg.port;
  $("password-state").textContent = state.password_set ? "A password is set. Saving a new one applies to the next connection." : "No password set yet.";
}

function renderTrusted(cfg) {
  if (renamingId) return; // don't clobber an open rename field
  const items = cfg.trusted.map((t) =>
    h("li", {},
      h("div", { class: "who" },
        h("b", {}, t.name),
        h("span", { class: "meta" }, `${idKind(t.id)} · added ${ago(t.added)} · last connected ${ago(t.last_seen)}`)),
      h("div", { class: "actions" },
        h("button", { class: "ghost", onclick: (e) => startRename(e.target.closest("li"), t) }, "Rename"),
        h("button", { class: "danger", onclick: () => act("POST", "trusted/remove", { id: t.id }, `${t.name} removed`) }, "Remove"))));
  $("trusted").replaceChildren(...(items.length ? items : [h("li", { class: "empty" }, "No trusted devices yet. Choose “Always allow” when a device asks to connect.")]));
}

function startRename(li, t) {
  renamingId = t.id;
  const input = h("input", { type: "text", value: t.name, maxlength: "64", "aria-label": "Device name" });
  const done = () => { renamingId = null; render(); };
  const save = async () => { renamingId = null; await act("POST", "trusted/rename", { id: t.id, name: input.value }, "Renamed"); };
  input.addEventListener("keydown", (e) => { if (e.key === "Enter") save(); if (e.key === "Escape") done(); });
  li.replaceChildren(
    h("div", { class: "inline", style: "flex:1" }, input),
    h("div", { class: "actions" }, h("button", { class: "primary", onclick: save }, "Save"), h("button", { class: "ghost", onclick: done }, "Cancel")));
  input.focus();
  input.select();
}

function renderStartup() {
  const a = state.autostart;
  $("autostart").checked = a.enabled;
  $("service-state").textContent = a.running_as_service
    ? "Running as a background service now."
    : a.enabled ? "Enabled. It will run as a service from your next login." : "Remote Desk only runs while you start it yourself.";
}

// ---------- controls ----------
for (const el of document.querySelectorAll("[data-setting]")) {
  el.addEventListener("change", () => act("PUT", "settings", { [el.dataset.setting]: el.checked }, "Saved"));
}
for (const el of document.querySelectorAll("[data-net]")) {
  el.addEventListener("change", () => {
    if (el.dataset.net === "internet" && el.checked && !confirm("Allow connections from the whole internet? VNC traffic is not encrypted, and the password is limited to 8 characters. Tailscale is the safer way to reach this computer from outside.")) {
      el.checked = false;
      return;
    }
    act("PUT", "settings", { allow_from: { ...state.config.allow_from, [el.dataset.net]: el.checked } }, "Saved");
  });
}
$("grace").addEventListener("change", (e) => act("PUT", "settings", { reconnect_grace_secs: Number(e.target.value) }, "Saved"));
$("port-form").addEventListener("submit", (e) => {
  e.preventDefault();
  act("PUT", "settings", { port: Number($("port").value) }, "Port changed. VNC apps must use the new port.");
});
$("password-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const input = $("password");
  if (await act("POST", "password", { password: input.value }, "Password saved")) input.value = "";
});
$("password-toggle").addEventListener("click", (e) => {
  const input = $("password");
  const show = input.type === "password";
  input.type = show ? "text" : "password";
  e.target.textContent = show ? "Hide" : "Show";
});
$("autostart").addEventListener("change", (e) =>
  act("POST", "autostart", { enabled: e.target.checked }, e.target.checked ? "Will start at login" : "Won't start at login"));

// ---------- polling ----------
async function refresh() {
  try {
    state = await api("GET", "state");
    render();
  } catch (e) {
    const pill = $("status-pill");
    pill.className = "pill bad";
    pill.textContent = token ? "Not running" : "Locked";
    if (!token) $("alerts").replaceChildren(h("div", { class: "banner warn" }, h("span", {}, e.message)));
  }
}

let initialTab = "overview";
try { initialTab = localStorage.getItem("rd-tab") || "overview"; } catch { /* ignore */ }
const tabParam = location.hash.match(/tab=(\w+)/);
if (tabParam) { initialTab = tabParam[1]; history.replaceState(null, "", "/"); }
showTab(document.querySelector(`[data-tab="${initialTab}"]`) ? initialTab : "overview");
if (location.hash) history.replaceState(null, "", "/"); // don't leave the token in the address bar
refresh();
setInterval(refresh, 1500);
window.addEventListener("resize", () => { lastLayoutKey = ""; if (state) renderDisplay(); });
