//! Connection to KWin: monitor layout, screencast streams and input injection.
//!
//! KWin only exposes `zkde_screencast_unstable_v1` and `org_kde_kwin_fake_input`
//! to executables whose .desktop file lists them in `X-KDE-Wayland-Interfaces`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

use crate::keymap::KeyTable;
use crate::proto::fake_input::org_kde_kwin_fake_input::{self, OrgKdeKwinFakeInput};
use crate::proto::screencast::zkde_screencast_stream_unstable_v1::{self as sstream, ZkdeScreencastStreamUnstableV1};
use crate::proto::screencast::zkde_screencast_unstable_v1::{self as screencast, ZkdeScreencastUnstableV1};

const POINTER_EMBEDDED: u32 = 2;

/// Name of the virtual output we create when no monitor is connected
/// (KWin may prefix it, e.g. "Virtual-remote-desk").
const VIRTUAL_OUTPUT: &str = "remote-desk";

/// A monitor in compositor (logical) coordinates.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Output {
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub pixel_width: i32,
    pub pixel_height: i32,
}

impl Output {
    /// Whether this is the virtual output we created ourselves.
    pub fn is_ours(&self) -> bool {
        self.name.contains(VIRTUAL_OUTPUT)
    }

    pub fn rect(&self) -> Rect {
        Rect { x: self.x, y: self.y, width: self.width, height: self.height }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug)]
pub enum Event {
    OutputsChanged,
    StreamReady { id: u64, node: u32 },
    StreamFailed { id: u64, error: String },
    StreamClosed { id: u64 },
    CaptureEnded { id: u64 },
    Disconnected,
}

pub struct Stream {
    pub id: u64,
    proxy: ZkdeScreencastStreamUnstableV1,
}

pub struct Desktop {
    conn: Connection,
    qh: QueueHandle<State>,
    screencast: ZkdeScreencastUnstableV1,
    fake_input: OrgKdeKwinFakeInput,
    keysyms: bool,
    outputs: Arc<Mutex<Vec<Output>>>,
    keys: Arc<Mutex<Option<Arc<KeyTable>>>>,
    next_id: AtomicU64,
    tx: Sender<Event>,
}

impl Desktop {
    /// Connects to the user's compositor, or to the Wayland socket `socket` in `$XDG_RUNTIME_DIR`.
    pub fn connect(socket: Option<&str>) -> Result<(Arc<Desktop>, Receiver<Event>)> {
        let conn = match socket {
            None => Connection::connect_to_env().context("cannot connect to the Wayland compositor")?,
            Some(name) => {
                let dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
                let stream = std::os::unix::net::UnixStream::connect(std::path::Path::new(&dir).join(name))
                    .with_context(|| format!("cannot connect to Wayland socket {name}"))?;
                Connection::from_socket(stream)?
            }
        };
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let (tx, rx) = mpsc::channel();
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(None));
        let mut state = State {
            keys: keys.clone(),
            seat: None,
            keyboard: None,
            _registry: conn.display().get_registry(&qh, ()),
            xdg_manager: None,
            outputs: Vec::new(),
            screencast: None,
            fake_input: None,
            shared: outputs.clone(),
            tx: tx.clone(),
        };
        // Globals, then output properties and seat capabilities, then the keymap.
        for _ in 0..4 {
            queue.roundtrip(&mut state)?;
        }
        let (Some(screencast), Some(fake_input)) = (state.screencast.clone(), state.fake_input.clone()) else {
            bail!(
                "KWin did not grant screencast/input access. Make sure the remote-desk .desktop file \
                 lists X-KDE-Wayland-Interfaces and its Exec= points at this binary"
            );
        };
        if keys.lock().unwrap().is_none() {
            log::warn!("no keymap from the compositor yet; typing falls back to keysym injection");
        }
        if screencast.version() < 3 || fake_input.version() < 4 {
            bail!("KWin is too old (screencast v{}, fake_input v{})", screencast.version(), fake_input.version());
        }
        fake_input.authenticate("Remote Desk".into(), "Remote control of this desktop".into());
        let keysyms = fake_input.version() >= 6;
        conn.flush()?;

        let thread_tx = tx.clone();
        std::thread::Builder::new().name("wayland".into()).spawn(move || {
            loop {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    log::warn!("Wayland connection lost: {e}");
                    let _ = thread_tx.send(Event::Disconnected);
                    break;
                }
            }
        })?;

        let desktop = Desktop { conn, qh, screencast, fake_input, keysyms, outputs, keys, next_id: AtomicU64::new(1), tx };
        Ok((Arc::new(desktop), rx))
    }

    pub fn outputs(&self) -> Vec<Output> {
        self.outputs.lock().unwrap().clone()
    }

    pub fn events(&self) -> Sender<Event> {
        self.tx.clone()
    }

    /// Streams a region of the workspace, scaled by `scale` (1.0 = logical size).
    pub fn stream_region(&self, r: Rect, scale: f64) -> Result<Stream> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let proxy = self.screencast.stream_region(
            r.x,
            r.y,
            r.width as u32,
            r.height as u32,
            scale,
            POINTER_EMBEDDED,
            &self.qh,
            id,
        );
        self.conn.flush()?;
        Ok(Stream { id, proxy })
    }

    /// Creates a headless virtual output of the given logical size and streams it.
    pub fn stream_virtual(&self, width: i32, height: i32) -> Result<Stream> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let proxy = if self.screencast.version() >= 4 {
            self.screencast.stream_virtual_output_with_description(
                VIRTUAL_OUTPUT.into(),
                "Remote Desk virtual display".into(),
                width,
                height,
                1.0,
                POINTER_EMBEDDED,
                &self.qh,
                id,
            )
        } else {
            self.screencast.stream_virtual_output(VIRTUAL_OUTPUT.into(), width, height, 1.0, POINTER_EMBEDDED, &self.qh, id)
        };
        self.conn.flush()?;
        Ok(Stream { id, proxy })
    }

    pub fn close_stream(&self, stream: Stream) {
        stream.proxy.close();
        let _ = self.conn.flush();
    }

    pub fn pointer_to(&self, x: f64, y: f64) {
        self.fake_input.pointer_motion_absolute(x, y);
        let _ = self.conn.flush();
    }

    pub fn button(&self, code: u32, pressed: bool) {
        self.fake_input.button(code, pressed as u32);
        let _ = self.conn.flush();
    }

    /// `axis`: 0 = vertical, 1 = horizontal. One wheel notch is 15 units.
    pub fn axis(&self, axis: u32, value: f64) {
        self.fake_input.axis(axis, value);
        let _ = self.conn.flush();
    }

    /// The compositor's current keymap as a keysym lookup table.
    pub fn key_table(&self) -> Option<Arc<KeyTable>> {
        self.keys.lock().unwrap().clone()
    }

    pub fn apply(&self, actions: &[crate::keymap::Action]) {
        use crate::keymap::Action;
        for a in actions {
            match *a {
                Action::Key { code, pressed } => self.fake_input.keyboard_key(code, pressed as u32),
                Action::Tap(sym) if self.keysyms => {
                    self.fake_input.keyboard_keysym(sym, 1);
                    self.fake_input.keyboard_keysym(sym, 0);
                }
                Action::Tap(_) => {}
            }
        }
        let _ = self.conn.flush();
    }
}

struct OutputState {
    global: u32,
    wl: wl_output::WlOutput,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    info: Output,
}

struct State {
    _registry: wl_registry::WlRegistry,
    keys: Arc<Mutex<Option<Arc<KeyTable>>>>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: Vec<OutputState>,
    screencast: Option<ZkdeScreencastUnstableV1>,
    fake_input: Option<OrgKdeKwinFakeInput>,
    shared: Arc<Mutex<Vec<Output>>>,
    tx: Sender<Event>,
}

impl State {
    fn publish_outputs(&self) {
        let list: Vec<Output> = self
            .outputs
            .iter()
            .map(|o| o.info.clone())
            .filter(|o| !o.name.is_empty() && o.width > 0 && o.height > 0)
            .collect();
        let mut shared = self.shared.lock().unwrap();
        if *shared != list {
            *shared = list;
            let _ = self.tx.send(Event::OutputsChanged);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match e {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_output" => {
                    let wl: wl_output::WlOutput = reg.bind(name, version.min(4), qh, name);
                    let xdg = s.xdg_manager.as_ref().map(|m| m.get_xdg_output(&wl, qh, name));
                    s.outputs.push(OutputState { global: name, wl, xdg, info: Output::default() });
                }
                "zxdg_output_manager_v1" => {
                    let m: zxdg_output_manager_v1::ZxdgOutputManagerV1 = reg.bind(name, version.min(3), qh, ());
                    for o in &mut s.outputs {
                        o.xdg = Some(m.get_xdg_output(&o.wl, qh, o.global));
                    }
                    s.xdg_manager = Some(m);
                }
                "zkde_screencast_unstable_v1" => s.screencast = Some(reg.bind(name, version.min(5), qh, ())),
                "org_kde_kwin_fake_input" => s.fake_input = Some(reg.bind(name, version.min(6), qh, ())),
                "wl_seat" if s.seat.is_none() => s.seat = Some(reg.bind(name, version.min(7), qh, ())),
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = s.outputs.iter().position(|o| o.global == name) {
                    let o = s.outputs.remove(i);
                    if let Some(x) = o.xdg {
                        x.destroy();
                    }
                    if o.wl.version() >= 3 {
                        o.wl.release();
                    }
                    s.publish_outputs();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(s: &mut Self, _: &wl_output::WlOutput, e: wl_output::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = s.outputs.iter_mut().find(|o| o.global == *global) else { return };
        match e {
            wl_output::Event::Mode { flags, width, height, .. } => {
                if flags.into_result().is_ok_and(|f| f.contains(wl_output::Mode::Current)) {
                    o.info.pixel_width = width;
                    o.info.pixel_height = height;
                }
            }
            wl_output::Event::Name { name } => o.info.name = name,
            wl_output::Event::Description { description } => o.info.description = description,
            wl_output::Event::Done => s.publish_outputs(),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(s: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, e: zxdg_output_v1::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = s.outputs.iter_mut().find(|o| o.global == *global) else { return };
        match e {
            zxdg_output_v1::Event::LogicalPosition { x, y } => (o.info.x, o.info.y) = (x, y),
            zxdg_output_v1::Event::LogicalSize { width, height } => (o.info.width, o.info.height) = (width, height),
            _ => {}
        }
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, u64> for State {
    fn event(s: &mut Self, _: &ZkdeScreencastStreamUnstableV1, e: sstream::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        let ev = match e {
            sstream::Event::Created { node } => Event::StreamReady { id: *id, node },
            sstream::Event::Failed { error } => Event::StreamFailed { id: *id, error },
            sstream::Event::Closed => Event::StreamClosed { id: *id },
            _ => return,
        };
        let _ = s.tx.send(ev);
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(s: &mut Self, seat: &wl_seat::WlSeat, e: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities } = e {
            let has_kbd = capabilities.into_result().is_ok_and(|c| c.contains(wl_seat::Capability::Keyboard));
            if has_kbd && s.keyboard.is_none() {
                s.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

/// We never get keyboard focus (no surface); we only listen for the keymap.
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(s: &mut Self, _: &wl_keyboard::WlKeyboard, e: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let wl_keyboard::Event::Keymap { format, fd, size } = e else { return };
        if format.into_result().ok() != Some(wl_keyboard::KeymapFormat::XkbV1) {
            return;
        }
        // The fd is shared with every other client, file offset included, so read
        // it position-independently instead of with read().
        let mut buf = vec![0u8; size as usize];
        if let Err(e) = std::os::unix::fs::FileExt::read_exact_at(&std::fs::File::from(fd), &mut buf, 0) {
            log::warn!("could not read the keymap: {e}");
            return;
        }
        let text = String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string();
        match KeyTable::from_keymap_string(text) {
            Some(t) => *s.keys.lock().unwrap() = Some(Arc::new(t)),
            None => log::warn!("could not parse the compositor keymap"),
        }
    }
}

macro_rules! ignore_events {
    ($($ty:ty => $ev:ty),* $(,)?) => {$(
        impl Dispatch<$ty, ()> for State {
            fn event(_: &mut Self, _: &$ty, _: $ev, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    zxdg_output_manager_v1::ZxdgOutputManagerV1 => zxdg_output_manager_v1::Event,
    ZkdeScreencastUnstableV1 => screencast::Event,
    OrgKdeKwinFakeInput => org_kde_kwin_fake_input::Event,
);
