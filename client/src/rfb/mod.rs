//! RFB (VNC) 3.3 / 3.7 / 3.8 client.
//!
//! [`Session::start`] connects on a background thread. The UI polls
//! [`Session::events`], reads the shared [`Framebuffer`], and sends input
//! through the `Session` methods, which queue messages for a writer thread.

pub mod auth;
pub mod decode;
pub mod framebuffer;

use std::io::{BufReader, BufWriter, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use decode::{MAX_PAYLOAD, ReadExt, Zrle};
pub use framebuffer::{Cursor, Framebuffer, Rect};

const ENC_RAW: i32 = 0;
const ENC_COPYRECT: i32 = 1;
const ENC_RRE: i32 = 2;
const ENC_HEXTILE: i32 = 5;
const ENC_ZRLE: i32 = 16;
const ENC_CURSOR: i32 = -239;
const ENC_LAST_RECT: i32 = -224;
const ENC_DESKTOP_SIZE: i32 = -223;
const ENC_DESKTOP_NAME: i32 = -307;
const ENC_EXT_DESKTOP_SIZE: i32 = -308;

const ENCODINGS: &[i32] = &[
    ENC_ZRLE,
    ENC_HEXTILE,
    ENC_COPYRECT,
    ENC_RRE,
    ENC_RAW,
    ENC_CURSOR,
    ENC_LAST_RECT,
    ENC_DESKTOP_SIZE,
    ENC_EXT_DESKTOP_SIZE,
    ENC_DESKTOP_NAME,
];

/// Mouse button bits in a PointerEvent.
pub mod button {
    pub const LEFT: u8 = 1;
    pub const MIDDLE: u8 = 2;
    pub const RIGHT: u8 = 4;
    pub const WHEEL_UP: u8 = 8;
    pub const WHEEL_DOWN: u8 = 16;
    pub const WHEEL_LEFT: u8 = 32;
    pub const WHEEL_RIGHT: u8 = 64;
    pub const BACK: u8 = 128;
}

#[derive(Clone, Debug, Default)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    /// Used by account logins (macOS Screen Sharing, VeNCrypt Plain); ignored by VNC password auth.
    pub username: String,
    /// `None` asks through [`Event::NeedCredentials`] when the server wants one.
    pub password: Option<String>,
    /// Let other viewers stay connected (ClientInit shared flag).
    pub shared: bool,
}

#[derive(Clone, Debug)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug)]
pub enum Event {
    Status(String),
    /// The server wants a password (and a username if `username` is set). Answer with
    /// [`Session::answer_credentials`].
    NeedCredentials {
        username: bool,
    },
    /// The server accepted these credentials, so they are safe to remember.
    Authenticated {
        username: String,
        password: Option<String>,
    },
    Connected {
        name: String,
    },
    Clipboard(String),
    Bell,
    /// The connection ended. `error` is `None` when the person disconnected.
    Closed {
        error: Option<String>,
        auth_failed: bool,
    },
}

struct Shared {
    socket: Mutex<Option<TcpStream>>,
    stopping: AtomicBool,
}

pub struct Session {
    pub fb: Arc<Mutex<Framebuffer>>,
    events: Receiver<Event>,
    out: Sender<Vec<u8>>,
    creds: Sender<Option<Credentials>>,
    shared: Arc<Shared>,
}

type Waker = Arc<dyn Fn() + Send + Sync>;

impl Session {
    /// Starts connecting in the background. `wake` is called whenever there is
    /// something new to show (events or screen updates).
    pub fn start(opts: ConnectOptions, wake: impl Fn() + Send + Sync + 'static) -> Session {
        let fb = Arc::new(Mutex::new(Framebuffer::new(0, 0, String::new())));
        let (event_tx, events) = mpsc::channel();
        let (out, out_rx) = mpsc::channel();
        let (creds, creds_rx) = mpsc::channel();
        let shared = Arc::new(Shared { socket: Mutex::new(None), stopping: AtomicBool::new(false) });
        let wake: Waker = Arc::new(wake);
        let worker = Worker {
            opts,
            fb: fb.clone(),
            events: event_tx,
            out: out.clone(),
            creds: creds_rx,
            shared: shared.clone(),
            wake: wake.clone(),
        };
        std::thread::Builder::new().name("rfb".into()).spawn(move || worker.run(out_rx)).expect("spawn network thread");
        Session { fb, events, out, creds, shared }
    }

    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.try_iter()
    }

    pub fn answer_credentials(&self, creds: Option<Credentials>) {
        let _ = self.creds.send(creds);
    }

    pub fn key(&self, keysym: u32, down: bool) {
        let mut m = vec![4, down as u8, 0, 0];
        m.extend(keysym.to_be_bytes());
        let _ = self.out.send(m);
    }

    pub fn pointer(&self, buttons: u8, x: u16, y: u16) {
        let mut m = vec![5, buttons];
        m.extend(x.to_be_bytes());
        m.extend(y.to_be_bytes());
        let _ = self.out.send(m);
    }

    /// Sends local clipboard text. RFB clipboard text is Latin-1.
    pub fn clipboard(&self, text: &str) {
        let latin1: Vec<u8> = text.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect();
        let mut m = vec![6, 0, 0, 0];
        m.extend((latin1.len() as u32).to_be_bytes());
        m.extend(latin1);
        let _ = self.out.send(m);
    }

    pub fn disconnect(&self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        if let Some(s) = self.shared.socket.lock().unwrap().as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
        // Unblock a pending credentials prompt.
        let _ = self.creds.send(None);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.disconnect();
    }
}

struct Worker {
    opts: ConnectOptions,
    fb: Arc<Mutex<Framebuffer>>,
    events: Sender<Event>,
    out: Sender<Vec<u8>>,
    creds: Receiver<Option<Credentials>>,
    shared: Arc<Shared>,
    wake: Waker,
}

impl Worker {
    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
        (self.wake)();
    }

    fn stopping(&self) -> bool {
        self.shared.stopping.load(Ordering::SeqCst)
    }

    fn run(self, out_rx: Receiver<Vec<u8>>) {
        let result = self.session(out_rx);
        if let Some(s) = self.shared.socket.lock().unwrap().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        let auth_failed = matches!(&result, Err(e) if e.downcast_ref::<AuthFailed>().is_some());
        let error = match result {
            Err(_) if self.stopping() => None,
            Err(e) => Some(friendly_error(&e)),
            Ok(()) => (!self.stopping()).then(|| "The server closed the connection.".to_string()),
        };
        if let Some(e) = &error {
            log::info!("session ended: {e}");
        }
        self.emit(Event::Closed { error, auth_failed });
    }

    fn session(&self, out_rx: Receiver<Vec<u8>>) -> Result<()> {
        let target = format!("{}:{}", display_host(&self.opts.host), self.opts.port);
        self.emit(Event::Status(format!("Connecting to {target}…")));
        let mut s = connect(&self.opts.host, self.opts.port)?;
        *self.shared.socket.lock().unwrap() = Some(s.try_clone()?);
        if self.stopping() {
            bail!("cancelled");
        }
        s.set_nodelay(true)?;
        let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(15)).with_interval(Duration::from_secs(5));
        let _ = socket2::SockRef::from(&s).set_tcp_keepalive(&ka);
        s.set_read_timeout(Some(Duration::from_secs(30)))?;

        let (width, height, name) = self.handshake(&mut s)?;
        s.set_read_timeout(None)?;
        {
            let mut fb = self.fb.lock().unwrap();
            *fb = Framebuffer::new(width, height, name.clone());
        }

        // Writer thread: everything the client sends after the handshake goes through `out`.
        let mut writer = BufWriter::new(s.try_clone()?);
        std::thread::Builder::new().name("rfb writer".into()).spawn(move || {
            while let Ok(msg) = out_rx.recv() {
                if writer.write_all(&msg).is_err() {
                    break;
                }
                // Batch whatever else is already queued into one write.
                while let Ok(more) = out_rx.try_recv() {
                    if writer.write_all(&more).is_err() {
                        return;
                    }
                }
                if writer.flush().is_err() {
                    break;
                }
            }
        })?;

        let mut init = vec![0, 0, 0, 0];
        init.extend(decode::PIXEL_FORMAT);
        init.extend([2, 0]);
        init.extend((ENCODINGS.len() as u16).to_be_bytes());
        for e in ENCODINGS {
            init.extend(e.to_be_bytes());
        }
        init.extend(update_request(false, width, height));
        let _ = self.out.send(init);

        self.emit(Event::Connected { name });
        let mut r = BufReader::with_capacity(256 * 1024, s);
        self.message_loop(&mut r, (width, height))
    }

    /// Version, security and initialisation. Returns the screen size and desktop name.
    fn handshake(&self, s: &mut TcpStream) -> Result<(usize, usize, String)> {
        let mut v = [0u8; 12];
        s.read_exact(&mut v).context("the server did not send a VNC greeting")?;
        let text = String::from_utf8_lossy(&v);
        let Some(ver) = text.strip_prefix("RFB ").map(|t| t.trim_end()) else {
            bail!("this is not a VNC server (it said {:?})", text.trim_end());
        };
        let (major, minor) = ver.split_once('.').context("bad version")?;
        let (major, minor): (u32, u32) = (major.parse()?, minor.parse()?);
        if major != 3 {
            bail!("unsupported RFB version {ver}");
        }
        let minor = match minor {
            8.. => 8, // includes Apple's 3.889
            7 => 7,
            _ => 3,
        };
        s.write_all(format!("RFB 003.{minor:03}\n").as_bytes())?;
        log::info!("server speaks RFB {ver}, using 3.{minor}");

        let have_username = !self.opts.username.is_empty();
        let chosen = if minor >= 7 {
            let n = s.u8()?;
            if n == 0 {
                bail!("the server refused the connection: {}", s.string(4096)?);
            }
            let offered = s.read_vec(n as usize)?;
            log::info!("security types offered: {offered:?}");
            let Some(t) = auth::choose(&offered, have_username) else {
                let names: Vec<String> = offered.iter().map(|&t| auth::name(t)).collect();
                bail!("the server requires a login method this viewer does not support ({})", names.join(", "));
            };
            s.write_all(&[t])?;
            t
        } else {
            match s.u32()? {
                0 => bail!("the server refused the connection: {}", s.string(4096)?),
                t @ (1 | 2) => t as u8,
                t => bail!("the server requires an unsupported login method ({})", auth::name(t as u8)),
            }
        };

        let creds = match chosen {
            auth::NONE => None,
            auth::VNC => {
                let mut challenge = [0u8; 16];
                s.read_exact(&mut challenge)?;
                let c = self.credentials(false)?;
                s.write_all(&auth::vnc_response(&c.password, &challenge))?;
                Some(c)
            }
            auth::ARD => {
                let generator = s.read_vec(2)?;
                let len = s.u16()? as usize;
                if !(16..=1024).contains(&len) {
                    bail!("bad Apple authentication key size {len}");
                }
                let prime = s.read_vec(len)?;
                let server_key = s.read_vec(len)?;
                let c = self.credentials(true)?;
                self.emit(Event::Status("Signing in…".into()));
                s.write_all(&auth::ard_response(&generator, &prime, &server_key, &c.username, &c.password))?;
                Some(c)
            }
            auth::VENCRYPT => Some(self.vencrypt(s)?),
            _ => unreachable!(),
        };

        // SecurityResult: always in 3.8, otherwise only after a real authentication.
        if minor >= 8 || chosen != auth::NONE {
            // remote-desk and similar servers hold the result until someone at the
            // computer approves a new device, so say what we are waiting for.
            s.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut probe = [0u8; 1];
            if s.peek(&mut probe).is_err() {
                self.emit(Event::Status("Waiting for the remote computer to approve this device…".into()));
            }
            s.set_read_timeout(Some(Duration::from_secs(120)))?;
            match s.u32()? {
                0 => {}
                _ => {
                    let reason = if minor >= 8 { s.string(4096).unwrap_or_default() } else { String::new() };
                    if reason.is_empty() {
                        bail!(AuthFailed("Authentication failed.".into()));
                    }
                    bail!(AuthFailed(reason));
                }
            }
            s.set_read_timeout(Some(Duration::from_secs(30)))?;
        }
        self.emit(Event::Authenticated {
            username: creds.as_ref().map(|c| c.username.clone()).unwrap_or_default(),
            password: creds.map(|c| c.password),
        });

        s.write_all(&[self.opts.shared as u8])?;
        let width = s.u16()? as usize;
        let height = s.u16()? as usize;
        let _server_pf = s.read_vec(16)?;
        let name = s.string(4096)?;
        log::info!("connected to {name:?}, {width}x{height}");
        Ok((width, height, name))
    }

    fn vencrypt(&self, s: &mut TcpStream) -> Result<Credentials> {
        let (major, minor) = (s.u8()?, s.u8()?);
        if (major, minor) < (0, 2) {
            bail!("unsupported VeNCrypt version {major}.{minor}");
        }
        s.write_all(&[0, 2])?;
        if s.u8()? != 0 {
            bail!("the server rejected VeNCrypt 0.2");
        }
        let n = s.u8()?;
        let subtypes = (0..n).map(|_| s.u32()).collect::<std::io::Result<Vec<_>>>()?;
        log::info!("VeNCrypt subtypes offered: {subtypes:?}");
        if !subtypes.contains(&auth::VENCRYPT_PLAIN) {
            bail!("the server only allows TLS-encrypted logins (VeNCrypt), which this viewer does not support yet");
        }
        s.write_all(&auth::VENCRYPT_PLAIN.to_be_bytes())?;
        let c = self.credentials(true)?;
        s.write_all(&auth::vencrypt_plain(&c.username, &c.password))?;
        Ok(c)
    }

    /// The credentials given up front, or asks the UI for the missing ones.
    fn credentials(&self, need_username: bool) -> Result<Credentials> {
        if let Some(password) = &self.opts.password
            && (!need_username || !self.opts.username.is_empty())
        {
            return Ok(Credentials { username: self.opts.username.clone(), password: password.clone() });
        }
        self.emit(Event::NeedCredentials { username: need_username });
        match self.creds.recv() {
            Ok(Some(c)) => {
                self.emit(Event::Status("Authenticating…".into()));
                Ok(c)
            }
            _ => {
                self.shared.stopping.store(true, Ordering::SeqCst);
                bail!("cancelled")
            }
        }
    }

    fn message_loop(&self, r: &mut impl Read, (mut width, mut height): (usize, usize)) -> Result<()> {
        let mut zrle = Zrle::default();
        loop {
            match r.u8()? {
                0 => {
                    (width, height) = self.update(r, &mut zrle, (width, height))?;
                    (self.wake)();
                    let _ = self.out.send(update_request(true, width, height));
                }
                1 => {
                    // SetColourMapEntries: we always ask for true colour, so just skip it.
                    let _pad = r.u8()?;
                    let _first = r.u16()?;
                    let n = r.u16()? as usize;
                    r.skip(n * 6)?;
                }
                2 => self.emit(Event::Bell),
                3 => {
                    r.skip(3)?;
                    let len = r.u32()? as usize;
                    if len > MAX_PAYLOAD {
                        bail!("clipboard text too large");
                    }
                    let text: String = r.read_vec(len)?.into_iter().map(char::from).collect();
                    self.emit(Event::Clipboard(text));
                }
                t => bail!("the server sent an unknown message ({t})"),
            }
        }
    }

    /// Reads one FramebufferUpdate. Returns the (possibly new) screen size.
    fn update(
        &self,
        r: &mut impl Read,
        zrle: &mut Zrle,
        (mut width, mut height): (usize, usize),
    ) -> Result<(usize, usize)> {
        let _pad = r.u8()?;
        let count = r.u16()?;
        for _ in 0..count {
            let rect = Rect::new(r.u16()? as usize, r.u16()? as usize, r.u16()? as usize, r.u16()? as usize);
            let encoding = r.i32()?;
            match encoding {
                ENC_LAST_RECT => break,
                ENC_DESKTOP_SIZE => {
                    (width, height) = (rect.w, rect.h);
                    self.fb.lock().unwrap().resize(width, height);
                }
                ENC_EXT_DESKTOP_SIZE => {
                    let screens = r.u8()? as usize;
                    r.skip(3 + screens * 16)?;
                    // x is the reason, y the status; a non-zero status means a refused request.
                    if rect.y == 0 {
                        (width, height) = (rect.w, rect.h);
                        self.fb.lock().unwrap().resize(width, height);
                    }
                }
                ENC_DESKTOP_NAME => {
                    let name = r.string(4096)?;
                    self.fb.lock().unwrap().name = name;
                }
                ENC_CURSOR => {
                    let cursor = if rect.is_empty() { None } else { Some(decode::cursor(r, rect)?) };
                    let mut fb = self.fb.lock().unwrap();
                    fb.set_cursor(cursor);
                }
                ENC_COPYRECT => {
                    let (sx, sy) = (r.u16()? as usize, r.u16()? as usize);
                    if !rect.fits(width, height) || !Rect::new(sx, sy, rect.w, rect.h).fits(width, height) {
                        bail!("the server sent a rectangle outside the screen");
                    }
                    self.fb.lock().unwrap().copy_within(sx, sy, rect);
                }
                ENC_RAW | ENC_RRE | ENC_HEXTILE | ENC_ZRLE => {
                    if !rect.fits(width, height) {
                        bail!("the server sent a rectangle outside the screen");
                    }
                    if rect.is_empty() {
                        if encoding == ENC_ZRLE {
                            let len = r.u32()? as usize;
                            r.skip(len)?;
                        }
                        continue;
                    }
                    // Decode without holding the lock so the UI never waits on the network.
                    let pixels = match encoding {
                        ENC_RAW => decode::raw(r, rect)?,
                        ENC_RRE => decode::rre(r, rect)?,
                        ENC_HEXTILE => decode::hextile(r, rect)?,
                        _ => zrle.decode(r, rect)?,
                    };
                    self.fb.lock().unwrap().blit(rect, &pixels);
                }
                e => bail!("the server used an encoding this viewer did not ask for ({e})"),
            }
        }
        Ok((width, height))
    }
}

fn update_request(incremental: bool, width: usize, height: usize) -> Vec<u8> {
    let mut m = vec![3, incremental as u8, 0, 0, 0, 0];
    m.extend((width as u16).to_be_bytes());
    m.extend((height as u16).to_be_bytes());
    m
}

/// Wrong password or similar; the UI offers to try again.
#[derive(Debug)]
pub struct AuthFailed(pub String);

impl std::fmt::Display for AuthFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthFailed {}

fn friendly_error(e: &anyhow::Error) -> String {
    if let Some(a) = e.downcast_ref::<AuthFailed>() {
        return a.0.clone();
    }
    if let Some(io) = e.downcast_ref::<std::io::Error>() {
        use std::io::ErrorKind::*;
        match io.kind() {
            UnexpectedEof | ConnectionReset | ConnectionAborted => {
                return "The connection was closed by the remote computer.".into();
            }
            TimedOut | WouldBlock => return "The remote computer stopped responding.".into(),
            _ => {}
        }
    }
    let msg = format!("{e:#}");
    let mut chars = msg.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect::<String>() + if msg.ends_with('.') { "" } else { "." },
        None => "Unknown error.".into(),
    }
}

fn display_host(host: &str) -> String {
    if host.contains(':') { format!("[{host}]") } else { host.to_string() }
}

fn connect(host: &str, port: u16) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> =
        (host, port).to_socket_addrs().with_context(|| format!("could not find the computer \"{host}\""))?.collect();
    let mut last = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, Duration::from_secs(10)) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    let e = last.map(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => {
            anyhow::anyhow!("{host} refused the connection on port {port}. Is the VNC server running?")
        }
        std::io::ErrorKind::TimedOut => anyhow::anyhow!("{host} did not answer on port {port} (timed out)"),
        _ => anyhow::anyhow!("could not connect to {host}:{port}: {e}"),
    });
    Err(e.unwrap_or_else(|| anyhow::anyhow!("no addresses found for {host}")))
}

/// Parses "host", "host:port", "host::port", "[v6]:port" or a bare IPv6 address.
/// A number below 100 after a single colon is a VNC display (5900 + n).
pub fn parse_address(input: &str, default_port: u16) -> Result<(String, u16), String> {
    let s = input.trim().trim_start_matches("vnc://").trim_end_matches('/');
    if s.is_empty() {
        return Err("Enter an IP address or host name.".into());
    }
    let port = |p: &str, display: bool| -> Result<u16, String> {
        let n: u16 = p.parse().map_err(|_| format!("\"{p}\" is not a valid port."))?;
        Ok(if display && n < 100 { 5900 + n } else { n })
    };
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or("Missing \"]\" after the IPv6 address.")?;
        let p = match after.strip_prefix("::").or_else(|| after.strip_prefix(':')) {
            Some(p) => port(p, !after.starts_with("::"))?,
            None if after.is_empty() => default_port,
            None => return Err("Unexpected text after the IPv6 address.".into()),
        };
        return Ok((host.to_string(), p));
    }
    if s.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok((s.to_string(), default_port));
    }
    if let Some((host, p)) = s.split_once("::") {
        return Ok((host.to_string(), port(p, false)?));
    }
    if let Some((host, p)) = s.split_once(':') {
        if host.is_empty() {
            return Err("Enter an IP address or host name.".into());
        }
        return Ok((host.to_string(), port(p, true)?));
    }
    Ok((s.to_string(), default_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(parse_address("10.0.0.5", 5900), Ok(("10.0.0.5".into(), 5900)));
        assert_eq!(parse_address(" host.lan:5901 ", 5900), Ok(("host.lan".into(), 5901)));
        assert_eq!(parse_address("host:1", 5900), Ok(("host".into(), 5901)));
        assert_eq!(parse_address("host::22", 5900), Ok(("host".into(), 22)));
        assert_eq!(parse_address("vnc://host:5905", 5900), Ok(("host".into(), 5905)));
        assert_eq!(parse_address("fe80::1", 5900), Ok(("fe80::1".into(), 5900)));
        assert_eq!(parse_address("[fe80::1]:5902", 5900), Ok(("fe80::1".into(), 5902)));
        assert_eq!(parse_address("[::1]", 5900), Ok(("::1".into(), 5900)));
        assert!(parse_address("", 5900).is_err());
        assert!(parse_address("host:abc", 5900).is_err());
        assert!(parse_address(":5900", 5900).is_err());
    }

    #[test]
    fn errors_read_like_sentences() {
        let e = anyhow::anyhow!("could not find the computer \"x\"");
        assert_eq!(friendly_error(&e), "Could not find the computer \"x\".");
        let e = anyhow::Error::new(AuthFailed("Authentication failed".into()));
        assert_eq!(friendly_error(&e), "Authentication failed");
    }
}
