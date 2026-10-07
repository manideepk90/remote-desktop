//! Keeps a screencast running and feeds frames into the [`FrameStore`].
//!
//! The controller loop owns the whole lifecycle: it (re)connects to KWin, picks
//! the region to stream, consumes the PipeWire node and restarts everything when
//! monitors change, the stream dies, the compositor restarts or settings change.
//! The virtual monitor and the private session only run while devices use them.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::VideoFormat;

use crate::desktop::{Desktop, Event, Output, Rect};
use crate::frame::FrameStore;
use crate::session::Session;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Output name, or `None` for all monitors.
    pub source: Option<String>,
    /// Scale applied by the compositor (1.0 = native logical size).
    pub scale: f64,
    pub max_fps: u32,
    /// Stream a virtual monitor of this logical size instead of the real ones.
    pub virtual_size: Option<(i32, i32)>,
    /// Stream a private headless session of this size instead of the user's desktop.
    pub session_size: Option<(i32, i32)>,
    /// What happens to the virtual monitor or private session once nobody is connected.
    pub keep: Keep,
}

impl Settings {
    /// Whether capture only runs while devices are connected.
    fn on_demand(&self) -> bool {
        self.virtual_size.is_some() || self.session_size.is_some()
    }

    fn backend(&self) -> Backend {
        match self.session_size {
            Some(size) => Backend::Session(size),
            None => Backend::Main,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Keep {
    /// Stop as soon as the last device disconnects.
    Stop,
    /// Keep running for this long after the last device disconnects.
    For(Duration),
    /// Keep running until the server stops or the setting changes.
    Always,
}

/// Which compositor we capture from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Backend {
    Main,
    Session((i32, i32)),
}

/// What clients need to map pointer coordinates back onto the desktop.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub region: Rect,
}

#[derive(Default)]
struct Demand {
    clients: usize,
    /// When the last device left; `None` if nobody has connected since the mode was chosen.
    idle_since: Option<Instant>,
}

pub struct Capture {
    pub store: Arc<FrameStore>,
    settings: Mutex<Settings>,
    restart: AtomicBool,
    desktop: Mutex<Option<Arc<Desktop>>>,
    geometry: Mutex<Option<Geometry>>,
    status: Mutex<String>,
    demand: Mutex<Demand>,
    demand_cv: Condvar,
}

/// Keeps on-demand capture running while a client holds it.
pub struct Attached(Arc<Capture>);

impl Drop for Attached {
    fn drop(&mut self) {
        let mut d = self.0.demand.lock().unwrap();
        d.clients -= 1;
        if d.clients == 0 {
            d.idle_since = Some(Instant::now());
        }
        self.0.demand_cv.notify_all();
    }
}

impl Capture {
    pub fn start(settings: Settings) -> Arc<Capture> {
        let cap = Arc::new(Capture {
            store: Arc::default(),
            settings: Mutex::new(settings),
            restart: AtomicBool::new(false),
            desktop: Mutex::new(None),
            geometry: Mutex::new(None),
            status: Mutex::new("starting".into()),
            demand: Mutex::default(),
            demand_cv: Condvar::new(),
        });
        let c = cap.clone();
        std::thread::Builder::new().name("capture".into()).spawn(move || c.run()).unwrap();
        cap
    }

    pub fn update_settings(&self, s: Settings) {
        let mut cur = self.settings.lock().unwrap();
        if *cur != s {
            if cur.backend() != s.backend() || cur.virtual_size.is_some() != s.virtual_size.is_some() {
                // A new mode starts idle until a device connects.
                let mut d = self.demand.lock().unwrap();
                if d.clients == 0 {
                    d.idle_since = None;
                }
            }
            *cur = s;
            self.restart.store(true, Ordering::SeqCst);
            self.demand_cv.notify_all();
        }
    }

    /// Marks a client as connected until the returned guard is dropped.
    pub fn attach(self: &Arc<Self>) -> Attached {
        self.demand.lock().unwrap().clients += 1;
        self.demand_cv.notify_all();
        Attached(self.clone())
    }

    pub fn desktop(&self) -> Option<Arc<Desktop>> {
        self.desktop.lock().unwrap().clone()
    }

    pub fn geometry(&self) -> Option<Geometry> {
        *self.geometry.lock().unwrap()
    }

    pub fn outputs(&self) -> Vec<Output> {
        self.desktop().map(|d| real_outputs(&d)).unwrap_or_default()
    }

    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }

    fn set_status(&self, s: impl Into<String>) {
        let s = s.into();
        let mut cur = self.status.lock().unwrap();
        if *cur != s {
            log::info!("capture: {s}");
            *cur = s;
        }
    }

    fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Whether capture should be running right now under these settings.
    fn wanted(&self, settings: &Settings) -> bool {
        if !settings.on_demand() {
            return true;
        }
        let d = self.demand.lock().unwrap();
        match (d.clients, d.idle_since, settings.keep) {
            (1.., _, _) => true,
            (0, None, _) | (0, Some(_), Keep::Stop) => false,
            (0, Some(t), Keep::For(keep)) => t.elapsed() < keep,
            (0, Some(_), Keep::Always) => true,
        }
    }

    fn run(self: Arc<Self>) {
        // The user's own desktop connection is kept across mode switches.
        let mut main: Option<(Arc<Desktop>, Receiver<Event>)> = None;
        let mut backoff = Duration::from_millis(500);
        loop {
            let settings = self.settings();
            if !self.wanted(&settings) {
                self.set_status("idle; starts when a device connects");
                let d = self.demand.lock().unwrap();
                let _ = self.demand_cv.wait_timeout(d, Duration::from_secs(1)).unwrap();
                continue;
            }
            let ok = match settings.backend() {
                Backend::Main => {
                    if main.is_none() {
                        match Desktop::connect(None) {
                            Ok(m) => main = Some(m),
                            Err(e) => self.set_status(format!("waiting for desktop: {e:#}")),
                        }
                    }
                    match &main {
                        Some((desktop, events)) => {
                            *self.desktop.lock().unwrap() = Some(desktop.clone());
                            if self.stream(desktop, events, Backend::Main) == Exit::Disconnected {
                                main = None;
                            }
                            true
                        }
                        None => false,
                    }
                }
                Backend::Session((w, h)) => {
                    self.set_status(format!("starting a private {w}x{h} session"));
                    match Session::start(w, h).and_then(|s| Ok((Desktop::connect(Some(&s.socket))?, s))) {
                        Ok(((desktop, events), session)) => {
                            *self.desktop.lock().unwrap() = Some(desktop.clone());
                            self.stream(&desktop, &events, Backend::Session((w, h)));
                            *self.desktop.lock().unwrap() = None;
                            drop(session);
                            true
                        }
                        Err(e) => {
                            self.set_status(format!("private session failed: {e:#}"));
                            false
                        }
                    }
                }
            };
            *self.geometry.lock().unwrap() = None;
            // Never show a new client the image of a screen that is no longer shared.
            self.store.clear();
            if ok {
                backoff = Duration::from_millis(500);
            } else {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }

    /// Streams from `desktop` until the compositor goes away, the mode changes or
    /// nobody needs the stream any more.
    fn stream(&self, desktop: &Arc<Desktop>, events: &Receiver<Event>, backend: Backend) -> Exit {
        let mut retry = Duration::from_millis(250);
        // Size for the virtual display used while no monitor is connected.
        let mut headless_size = (1920, 1080);
        while events.try_recv().is_ok() {} // drop events from while we were not streaming
        loop {
            self.restart.store(false, Ordering::SeqCst);
            let settings = self.settings();
            if settings.backend() != backend || !self.wanted(&settings) {
                return Exit::Switch;
            }
            // The monitor choice refers to the user's own desktop.
            let source = if backend == Backend::Main { settings.source.as_deref() } else { None };
            let target = match (settings.virtual_size, pick_region(&real_outputs(desktop), source)) {
                (Some((width, height)), _) if backend == Backend::Main => Target::Virtual { width, height },
                (_, Some(r)) => {
                    headless_size = (r.width, r.height);
                    Target::Region(r)
                }
                (_, None) => Target::Virtual { width: headless_size.0, height: headless_size.1 },
            };
            let stream = match target {
                Target::Region(r) => desktop.stream_region(r, settings.scale),
                Target::Virtual { width, height } => desktop.stream_virtual(width, height),
            };
            let Ok(stream) = stream else { return Exit::Disconnected };
            let id = stream.id;
            let mut running: Option<PwHandle> = None;
            let mut placed = false;
            let deadline = Instant::now() + Duration::from_secs(5);
            let reason = loop {
                if self.restart.load(Ordering::SeqCst) {
                    break "settings changed".to_string();
                }
                if !self.wanted(&settings) {
                    break "no devices connected".to_string();
                }
                if running.is_none() && Instant::now() > deadline {
                    break "timed out waiting for the compositor".to_string();
                }
                match events.recv_timeout(Duration::from_millis(200)) {
                    Ok(Event::StreamReady { id: i, node }) if i == id => {
                        if matches!(target, Target::Virtual { .. }) {
                            placed = place_virtual(desktop);
                        }
                        *self.geometry.lock().unwrap() = Some(Geometry { region: target.region(desktop) });
                        match start_pipewire(node, id, self.store.clone(), settings.max_fps, desktop.events()) {
                            Ok(h) => {
                                self.set_status(match target {
                                    _ if backend != Backend::Main => {
                                        let r = target.region(desktop);
                                        format!("streaming a private {}x{} session", r.width, r.height)
                                    }
                                    Target::Region(r) => format!("streaming {}x{} @ {}x", r.width, r.height, settings.scale),
                                    Target::Virtual { width, height } if settings.virtual_size.is_some() => {
                                        format!("streaming a {width}x{height} virtual monitor")
                                    }
                                    Target::Virtual { width, height } => {
                                        format!("no monitors connected; streaming a {width}x{height} virtual display")
                                    }
                                });
                                retry = Duration::from_millis(250);
                                running = Some(h);
                            }
                            Err(e) => break format!("PipeWire: {e:#}"),
                        }
                    }
                    Ok(Event::StreamFailed { id: i, error }) if i == id => break format!("stream failed: {error}"),
                    Ok(Event::StreamClosed { id: i }) if i == id => break "stream closed".into(),
                    Ok(Event::CaptureEnded { id: i }) if i == id => break "PipeWire stream ended".into(),
                    Ok(Event::OutputsChanged) => {
                        let now = pick_region(&real_outputs(desktop), source);
                        match target {
                            Target::Region(r) if now != Some(r) => break "monitor layout changed".into(),
                            Target::Virtual { .. } if now.is_some() && settings.virtual_size.is_none() => {
                                break "monitor connected".into();
                            }
                            // Our virtual output appeared or moved: keep pointer mapping in sync.
                            Target::Virtual { .. } if running.is_some() => {
                                if !placed {
                                    placed = place_virtual(desktop);
                                }
                                *self.geometry.lock().unwrap() = Some(Geometry { region: target.region(desktop) });
                            }
                            _ => {}
                        }
                    }
                    Ok(Event::Disconnected) | Err(RecvTimeoutError::Disconnected) => {
                        if let Some(h) = running.take() {
                            h.stop();
                        }
                        self.set_status("compositor disconnected");
                        return Exit::Disconnected;
                    }
                    Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                }
            };
            if let Some(h) = running.take() {
                h.stop();
            }
            desktop.close_stream(stream);
            if !self.wanted(&self.settings()) || self.settings().backend() != backend {
                return Exit::Switch;
            }
            self.set_status(format!("restarting capture: {reason}"));
            std::thread::sleep(retry);
            retry = (retry * 2).min(Duration::from_secs(3));
        }
    }
}

#[derive(PartialEq)]
enum Exit {
    /// The compositor connection is gone.
    Disconnected,
    /// The mode changed or the stream is no longer needed.
    Switch,
}

#[derive(Clone, Copy)]
enum Target {
    /// A region of the real monitors.
    Region(Rect),
    /// A headless output we create because no monitor is connected.
    Virtual { width: i32, height: i32 },
}

impl Target {
    /// Where the stream sits in compositor coordinates.
    fn region(self, desktop: &Desktop) -> Rect {
        match self {
            Target::Region(r) => r,
            Target::Virtual { width, height } => desktop
                .outputs()
                .iter()
                .find(|o| o.is_ours())
                .map(Output::rect)
                .unwrap_or(Rect { x: 0, y: 0, width, height }),
        }
    }
}

/// KWin may put a new virtual output on top of a real monitor (so it just mirrors
/// that monitor) or make it the primary display. Moves it to the right of the real
/// monitors and last in priority. Returns false while our output has not appeared yet.
fn place_virtual(desktop: &Desktop) -> bool {
    let outputs = desktop.outputs();
    let Some(ours) = outputs.iter().find(|o| o.is_ours()) else { return false };
    let real: Vec<&Output> = outputs.iter().filter(|o| !o.is_ours()).collect();
    let (Some(right), Some(top)) = (real.iter().map(|o| o.x + o.width).max(), real.iter().map(|o| o.y).min()) else {
        return true; // it is the only output
    };
    let kscreen = |arg: String| match std::process::Command::new("kscreen-doctor").arg(&arg).output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            log::warn!("kscreen-doctor {arg}: {}", String::from_utf8_lossy(&out.stderr).trim());
            false
        }
        Err(e) => {
            log::warn!("cannot run kscreen-doctor to place the virtual monitor: {e}");
            false
        }
    };
    if real.iter().any(|o| overlaps(o.rect(), ours.rect())) {
        log::info!("moving the virtual monitor to {right},{top}, beside the real monitors");
        kscreen(format!("output.{}.position.{right},{top}", ours.name));
    }
    kscreen(format!("output.{}.priority.{}", ours.name, real.len() + 1));
    true
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

/// Physical monitors, without the virtual output we may have created.
fn real_outputs(desktop: &Desktop) -> Vec<Output> {
    desktop.outputs().into_iter().filter(|o| !o.is_ours()).collect()
}

/// The logical region to stream: one named output, or the bounding box of all of them.
pub fn pick_region(outputs: &[Output], source: Option<&str>) -> Option<Rect> {
    let chosen: Vec<&Output> = match source.and_then(|n| outputs.iter().find(|o| o.name == n)) {
        Some(o) => vec![o],
        None => outputs.iter().collect(),
    };
    let x0 = chosen.iter().map(|o| o.x).min()?;
    let y0 = chosen.iter().map(|o| o.y).min()?;
    let x1 = chosen.iter().map(|o| o.x + o.width).max()?;
    let y1 = chosen.iter().map(|o| o.y + o.height).max()?;
    Some(Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
}

struct PwHandle {
    quit: pw::channel::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl PwHandle {
    fn stop(self) {
        let _ = self.quit.send(());
        let _ = self.thread.join();
    }
}

fn start_pipewire(node: u32, id: u64, store: Arc<FrameStore>, max_fps: u32, events: Sender<Event>) -> Result<PwHandle> {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new().name("pipewire".into()).spawn(move || {
        let result = run_pipewire(node, store, max_fps, quit_rx, &ready_tx);
        if let Err(e) = &result {
            log::warn!("PipeWire: {e:#}");
            let _ = ready_tx.send(Err(format!("{e:#}")));
        }
        let _ = events.send(Event::CaptureEnded { id });
    })?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(PwHandle { quit: quit_tx, thread }),
        Ok(Err(e)) => Err(anyhow!(e)),
        Err(_) => {
            let _ = quit_tx.send(());
            Err(anyhow!("PipeWire did not start"))
        }
    }
}

#[derive(Default)]
struct StreamFormat {
    format: Cell<Option<VideoFormat>>,
    size: Cell<(u32, u32)>,
}

fn run_pipewire(
    node: u32,
    store: Arc<FrameStore>,
    max_fps: u32,
    quit: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "remote-desk",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let fmt = Rc::new(StreamFormat::default());
    let min_interval = Duration::from_secs_f64(1.0 / max_fps.max(1) as f64);
    let last = Cell::new(Instant::now() - min_interval);
    let ml_err = mainloop.clone();
    let ml_quit = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| ml_quit.quit());

    let _listener = stream
        .add_local_listener_with_user_data(fmt.clone())
        .state_changed(move |_, _, _, new| {
            if let pw::stream::StreamState::Error(e) = &new {
                log::warn!("PipeWire stream error: {e}");
                ml_err.quit();
            } else if matches!(new, pw::stream::StreamState::Unconnected) {
                ml_err.quit();
            }
        })
        .param_changed(|_, f, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_ok() {
                f.format.set(Some(info.format()));
                f.size.set((info.size().width, info.size().height));
                log::debug!("negotiated {:?} {}x{}", info.format(), info.size().width, info.size().height);
            }
        })
        .process(move |stream, f| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let now = Instant::now();
            if now.duration_since(last.get()) < min_interval {
                return;
            }
            let (Some(format), (w, h)) = (f.format.get(), f.size.get()) else { return };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let (size, offset, stride) = (d.chunk().size() as usize, d.chunk().offset() as usize, d.chunk().stride());
            if size == 0 {
                return; // cursor-only or empty update
            }
            let Some(bytes) = d.data() else {
                log::warn!("received a buffer without CPU-mapped memory (type {:?})", d.type_());
                return;
            };
            let stride = if stride > 0 { stride as usize } else { w as usize * 4 };
            if let Some(packed) = to_bgrx(&bytes[offset.min(bytes.len())..], w, h, stride, format) {
                last.set(now);
                store.publish(w, h, packed);
            }
        })
        .register()?;

    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(spa::param::format::FormatProperties::MediaType, Id, spa::param::format::MediaType::Video),
        spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype, Id, spa::param::format::MediaSubtype::Raw),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 16384, height: 16384 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: max_fps, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    );
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
        .map_err(|e| anyhow!("pod: {e:?}"))?
        .0
        .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("bad pod"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// Repacks a frame into tightly packed BGRX.
fn to_bgrx(src: &[u8], w: u32, h: u32, stride: usize, format: VideoFormat) -> Option<Vec<u8>> {
    let row = w as usize * 4;
    if stride < row || src.len() < stride * (h as usize - 1) + row {
        return None;
    }
    let mut out = Vec::with_capacity(row * h as usize);
    let swap = matches!(format, VideoFormat::RGBx | VideoFormat::RGBA);
    for y in 0..h as usize {
        let line = &src[y * stride..y * stride + row];
        if swap {
            out.extend(line.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 0]));
        } else {
            out.extend_from_slice(line);
        }
    }
    Some(out)
}
