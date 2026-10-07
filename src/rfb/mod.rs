//! RFB (VNC) 3.3/3.7/3.8 server.

pub mod encode;

use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use des::Des;
use des::cipher::{BlockCipherEncrypt, KeyInit};

use crate::app::App;
use crate::frame::Frame;
use encode::{Encoder, PixelFormat, Rect, dirty_rects};

const ENC_RAW: i32 = 0;
const ENC_ZRLE: i32 = 16;
const ENC_DESKTOP_SIZE: i32 = -223;
const ENC_EXT_DESKTOP_SIZE: i32 = -308;
const ENC_DESKTOP_NAME: i32 = -307;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// Accepts connections forever, rebinding if the port changes.
pub fn serve(app: Arc<App>) {
    loop {
        let port = app.config().port;
        let listener = match bind(port) {
            Ok(l) => l,
            Err(e) => {
                app.set_listen_error(Some(format!("cannot listen on port {port}: {e:#}")));
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        app.set_listen_error(None);
        log::info!("listening on port {port}");
        listener.set_nonblocking(true).ok();
        while app.config().port == port {
            match listener.accept() {
                Ok((stream, peer)) => {
                    stream.set_nonblocking(false).ok();
                    let app = app.clone();
                    std::thread::Builder::new()
                        .name(format!("client {peer}"))
                        .spawn(move || {
                            if let Err(e) = handle(&app, stream, peer) {
                                log::info!("{peer}: {e:#}");
                            }
                        })
                        .ok();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => {
                    log::warn!("accept: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }
}

/// Dual-stack listener: IPv6 socket that also accepts IPv4, falling back to IPv4 only.
fn bind(port: u16) -> Result<TcpListener> {
    use socket2::{Domain, Socket, Type};
    let v6 = (|| -> std::io::Result<TcpListener> {
        let s = Socket::new(Domain::IPV6, Type::STREAM, None)?;
        s.set_only_v6(false)?;
        s.set_reuse_address(true)?;
        s.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
        s.listen(16)?;
        Ok(s.into())
    })();
    match v6 {
        Ok(l) => Ok(l),
        Err(_) => {
            let s = Socket::new(Domain::IPV4, Type::STREAM, None)?;
            s.set_reuse_address(true)?;
            s.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
            s.listen(16)?;
            Ok(s.into())
        }
    }
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn read_n<const N: usize>(s: &mut TcpStream) -> std::io::Result<[u8; N]> {
    let mut b = [0u8; N];
    s.read_exact(&mut b)?;
    Ok(b)
}

fn send_failure(s: &mut TcpStream, minor: u8, reason: &str) {
    let mut out = 1u32.to_be_bytes().to_vec();
    if minor >= 8 {
        out.extend((reason.len() as u32).to_be_bytes());
        out.extend(reason.as_bytes());
    }
    let _ = s.write_all(&out);
}

/// VNC authentication: DES-encrypt the challenge with the (bit-reversed) password.
pub fn vnc_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let des = Des::new_from_slice(&key).expect("8-byte key");
    let mut out = *challenge;
    for block in out.chunks_exact_mut(8) {
        let mut b: [u8; 8] = block.try_into().unwrap();
        des.encrypt_block((&mut b).into());
        block.copy_from_slice(&b);
    }
    out
}

fn handle(app: &Arc<App>, mut s: TcpStream, peer: SocketAddr) -> Result<()> {
    let ip = canonical_ip(peer.ip());
    if !app.peer_allowed(ip) {
        bail!("rejected: {ip} is not in an allowed network");
    }
    s.set_nodelay(true)?;
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5))
        .with_retries(3);
    socket2::SockRef::from(&s).set_tcp_keepalive(&ka)?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;

    // Protocol version.
    s.write_all(b"RFB 003.008\n")?;
    let v: [u8; 12] = read_n(&mut s)?;
    let minor = match &v {
        b"RFB 003.008\n" => 8,
        b"RFB 003.007\n" => 7,
        v if v.starts_with(b"RFB 003.") => 3, // 3.3 and the odd 3.5/3.889 variants
        _ => bail!("not a VNC client"),
    };

    if let Some(wait) = app.lockout_remaining(ip) {
        let msg = format!("Too many failed attempts, try again in {}s", wait.as_secs().max(1));
        if minor >= 7 {
            s.write_all(&[0])?;
            s.write_all(&(msg.len() as u32).to_be_bytes())?;
            s.write_all(msg.as_bytes())?;
        } else {
            s.write_all(&0u32.to_be_bytes())?;
        }
        bail!("locked out");
    }

    // Security: VNC authentication only.
    if minor >= 7 {
        s.write_all(&[1, 2])?;
        let [chosen] = read_n::<1>(&mut s)?;
        if chosen != 2 {
            send_failure(&mut s, minor, "Unsupported security type");
            bail!("client chose security type {chosen}");
        }
    } else {
        s.write_all(&2u32.to_be_bytes())?;
    }
    let challenge: [u8; 16] = rand::random();
    s.write_all(&challenge)?;
    let response: [u8; 16] = read_n(&mut s)?;
    let password = app.config().password;
    if password.is_empty() {
        send_failure(&mut s, minor, "No password is set yet. Open Remote Desk settings on the host to set one.");
        bail!("rejected: no password configured");
    }
    if response != vnc_response(&password, &challenge) {
        app.auth_failed(ip);
        send_failure(&mut s, minor, "Authentication failed");
        bail!("wrong password");
    }
    app.auth_succeeded(ip);

    // Pairing / approval (may wait for the person at the desk).
    let device = app.identify(ip);
    if let Err(reason) = app.pairing.authorize(app, &device) {
        send_failure(&mut s, minor, &reason);
        bail!("{} not approved: {reason}", device.name);
    }
    s.write_all(&0u32.to_be_bytes())?;

    // Initialisation. A virtual monitor or private session starts now if needed.
    let _attached = app.capture.attach();
    let _shared: [u8; 1] = read_n(&mut s)?;
    s.set_read_timeout(None)?;
    let frame = app
        .capture
        .store
        .latest()
        .or_else(|| app.capture.store.wait_newer(0, Duration::from_secs(30)))
        .context("no screen image available")?;
    let mut init = Vec::new();
    init.extend((frame.width as u16).to_be_bytes());
    init.extend((frame.height as u16).to_be_bytes());
    PixelFormat::default().write(&mut init);
    let name = app.desktop_name();
    init.extend((name.len() as u32).to_be_bytes());
    init.extend(name.as_bytes());
    s.write_all(&init)?;

    let session = app.sessions.add(&device, s.try_clone()?);
    log::info!("{} ({ip}) connected", device.name);
    app.pairing.notify_connected(&device);

    let shared = Arc::new(Shared {
        st: Mutex::new(ClientState { fb: (frame.width, frame.height), ..Default::default() }),
        cv: Condvar::new(),
        closed: AtomicBool::new(false),
    });
    let writer = {
        let (app, shared, out, id) = (app.clone(), shared.clone(), s.try_clone()?, session.id);
        std::thread::Builder::new()
            .name(format!("updates {peer}"))
            .spawn(move || {
                if let Err(e) = write_updates(&app, &shared, out, frame, id) {
                    log::debug!("{peer} writer: {e:#}");
                }
                shared.close();
            })?
    };
    let result = read_messages(app, &shared, &mut s, session.id).or_else(|e| {
        let closed = e.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(io.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset)
        });
        if closed || shared.is_closed() { Ok(()) } else { Err(e) }
    });
    shared.close();
    let _ = s.shutdown(Shutdown::Both);
    let _ = writer.join();
    app.sessions.remove(session.id);
    log::info!("{} ({ip}) disconnected", device.name);
    app.pairing.notify_disconnected(&device);
    result
}

#[derive(Default)]
struct ClientState {
    pf: Option<PixelFormat>,
    encodings: Vec<i32>,
    request: Option<bool>, // Some(incremental)
    fb: (u32, u32),
    resize_refused: bool,
}

struct Shared {
    st: Mutex<ClientState>,
    cv: Condvar,
    closed: AtomicBool,
}

impl Shared {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Keys and buttons this session is holding down on the desktop.
#[derive(Default)]
struct Input {
    keyboard: crate::keymap::Keyboard,
    buttons: u16,
}

impl Input {
    /// Lets go of everything, so nothing stays stuck when a client leaves or loses control.
    fn release_all(&mut self, app: &App) {
        let Some(d) = app.capture.desktop() else { return };
        d.apply(&self.keyboard.release_all());
        for (bit, code) in [(0, BTN_LEFT), (1, BTN_MIDDLE), (2, BTN_RIGHT), (7, BTN_SIDE), (8, BTN_EXTRA)] {
            if self.buttons & (1 << bit) != 0 {
                d.button(code, false);
            }
        }
        self.buttons = 0;
    }
}

fn read_messages(app: &Arc<App>, sh: &Shared, s: &mut TcpStream, session: u64) -> Result<()> {
    let mut input = Input::default();
    let result = message_loop(app, sh, s, session, &mut input);
    input.release_all(app);
    result
}

fn message_loop(app: &Arc<App>, sh: &Shared, s: &mut TcpStream, session: u64, input: &mut Input) -> Result<()> {
    let mut last_pos = (u16::MAX, u16::MAX);
    loop {
        let [kind] = read_n::<1>(s)?;
        match kind {
            0 => {
                let b: [u8; 19] = read_n(s)?;
                let pf = PixelFormat::parse(b[3..19].try_into().unwrap());
                if !pf.is_supported() {
                    bail!("unsupported pixel format {pf:?}");
                }
                sh.st.lock().unwrap().pf = Some(pf);
            }
            2 => {
                let b: [u8; 3] = read_n(s)?;
                let n = u16::from_be_bytes([b[1], b[2]]) as usize;
                let mut raw = vec![0u8; n * 4];
                s.read_exact(&mut raw)?;
                let encs = raw.chunks_exact(4).map(|c| i32::from_be_bytes(c.try_into().unwrap())).collect();
                sh.st.lock().unwrap().encodings = encs;
            }
            3 => {
                let b: [u8; 9] = read_n(s)?;
                let mut st = sh.st.lock().unwrap();
                let incremental = b[0] != 0;
                st.request = Some(st.request.map_or(incremental, |prev| prev && incremental));
                sh.cv.notify_all();
            }
            4 => {
                let b: [u8; 7] = read_n(s)?;
                let down = b[0] != 0;
                let keysym = u32::from_be_bytes([b[3], b[4], b[5], b[6]]);
                if !app.input_allowed(session) {
                    input.release_all(app);
                    continue;
                }
                if let Some(d) = app.capture.desktop() {
                    let table = d.key_table();
                    d.apply(&input.keyboard.event(table.as_deref(), keysym, down));
                }
            }
            5 => {
                let b: [u8; 5] = read_n(s)?;
                let mask = b[0] as u16;
                let (x, y) = (u16::from_be_bytes([b[1], b[2]]), u16::from_be_bytes([b[3], b[4]]));
                if !app.input_allowed(session) {
                    input.release_all(app);
                    continue;
                }
                let (Some(d), Some(g)) = (app.capture.desktop(), app.capture.geometry()) else { continue };
                if (x, y) != last_pos {
                    let (fw, fh) = sh.st.lock().unwrap().fb;
                    let gx = g.region.x as f64 + x as f64 * g.region.width as f64 / fw.max(1) as f64;
                    let gy = g.region.y as f64 + y as f64 * g.region.height as f64 / fh.max(1) as f64;
                    d.pointer_to(gx, gy);
                    last_pos = (x, y);
                }
                let changed = mask ^ input.buttons;
                for (bit, code) in [(0, BTN_LEFT), (1, BTN_MIDDLE), (2, BTN_RIGHT), (7, BTN_SIDE), (8, BTN_EXTRA)] {
                    if changed & (1 << bit) != 0 {
                        d.button(code, mask & (1 << bit) != 0);
                    }
                }
                // Wheel "buttons" fire on press.
                let pressed = changed & mask;
                for (bit, axis, dir) in [(3, 0, -1.0), (4, 0, 1.0), (5, 1, -1.0), (6, 1, 1.0)] {
                    if pressed & (1 << bit) != 0 {
                        d.axis(axis, dir * 15.0);
                    }
                }
                input.buttons = mask;
            }
            6 => {
                let b: [u8; 7] = read_n(s)?;
                let len = i32::from_be_bytes([b[3], b[4], b[5], b[6]]).unsigned_abs() as usize;
                if len > 16 << 20 {
                    bail!("clipboard too large");
                }
                let mut text = vec![0u8; len];
                s.read_exact(&mut text)?;
            }
            251 => {
                // SetDesktopSize: the server controls resolution; refuse politely.
                let b: [u8; 7] = read_n(s)?;
                let screens = b[5] as usize;
                let mut skip = vec![0u8; screens * 16];
                s.read_exact(&mut skip)?;
                sh.st.lock().unwrap().resize_refused = true;
                sh.cv.notify_all();
            }
            k => bail!("unsupported client message {k}"),
        }
    }
}

fn write_updates(app: &App, sh: &Shared, mut out: TcpStream, first: Arc<Frame>, session: u64) -> Result<()> {
    let mut enc = Encoder::new();
    let mut last: Option<Arc<Frame>> = None;
    let mut fb = (first.width, first.height);
    let store = &app.capture.store;
    let mut buf = Vec::new();
    loop {
        // Wait for the client to ask for an update.
        let (incremental, encodings, refused) = {
            let mut st = sh.st.lock().unwrap();
            while st.request.is_none() && !st.resize_refused && !sh.is_closed() {
                st = sh.cv.wait_timeout(st, Duration::from_secs(1)).unwrap().0;
            }
            if sh.is_closed() {
                return Ok(());
            }
            if let Some(pf) = st.pf.take()
                && pf != enc.pf
            {
                enc.pf = pf;
                last = None;
            }
            let refused = std::mem::take(&mut st.resize_refused);
            (st.request, st.encodings.clone(), refused)
        };
        let has = |e: i32| encodings.contains(&e);
        let zrle = has(ENC_ZRLE);
        let ext_size = has(ENC_EXT_DESKTOP_SIZE);

        if refused && ext_size {
            // Reply to SetDesktopSize: reason 1 (client request), status 1 (prohibited).
            buf.clear();
            write_update_header(&mut buf, 1);
            write_ext_desktop_size(&mut buf, fb, 1, 1);
            out.write_all(&buf)?;
        }
        let Some(incremental) = incremental else { continue };

        // Wait for a frame worth sending.
        let frame = loop {
            if sh.is_closed() {
                return Ok(());
            }
            let latest = store.latest();
            let seen = last.as_ref().map_or(0, |f| f.seq);
            match latest {
                Some(f) if !incremental || last.is_none() || f.seq != seen => break f,
                _ => {
                    store.wait_newer(seen, Duration::from_millis(250));
                }
            }
        };

        let mut rects: Vec<Rect> = Vec::new();
        buf.clear();
        let mut pseudo = 0u16;
        let mut pseudo_buf = Vec::new();
        let full = Rect { x: 0, y: 0, w: frame.width, h: frame.height };
        if (frame.width, frame.height) != fb {
            if ext_size {
                write_ext_desktop_size(&mut pseudo_buf, (frame.width, frame.height), 0, 0);
            } else if has(ENC_DESKTOP_SIZE) {
                write_rect_header(&mut pseudo_buf, Rect { x: 0, y: 0, w: frame.width, h: frame.height }, ENC_DESKTOP_SIZE);
            } else {
                bail!("screen size changed and the client cannot resize; it should reconnect");
            }
            pseudo += 1;
            fb = (frame.width, frame.height);
            sh.st.lock().unwrap().fb = fb;
            rects.push(full);
            if has(ENC_DESKTOP_NAME) {
                let name = app.desktop_name();
                write_rect_header(&mut pseudo_buf, Rect { x: 0, y: 0, w: 0, h: 0 }, ENC_DESKTOP_NAME);
                pseudo_buf.extend((name.len() as u32).to_be_bytes());
                pseudo_buf.extend(name.as_bytes());
                pseudo += 1;
            }
        } else {
            match (&last, incremental) {
                (Some(prev), true) => rects = dirty_rects(prev, &frame),
                _ => rects.push(full),
            }
        }
        if rects.is_empty() && pseudo == 0 {
            last = Some(frame);
            continue; // nothing changed; keep the request pending
        }

        let started = std::time::Instant::now();
        write_update_header(&mut buf, rects.len() as u16 + pseudo);
        buf.extend_from_slice(&pseudo_buf);
        for r in &rects {
            if zrle {
                write_rect_header(&mut buf, *r, ENC_ZRLE);
                enc.zrle(&frame, *r, &mut buf);
            } else {
                write_rect_header(&mut buf, *r, ENC_RAW);
                enc.raw(&frame, *r, &mut buf);
            }
        }
        sh.st.lock().unwrap().request = None;
        let pixels: u64 = rects.iter().map(|r| r.w as u64 * r.h as u64).sum();
        log::debug!(
            "update: {} rects, {} px, {} bytes ({:.1}% of raw), encoded in {:.1} ms",
            rects.len(),
            pixels,
            buf.len(),
            100.0 * buf.len() as f64 / (pixels.max(1) * 4) as f64,
            started.elapsed().as_secs_f64() * 1000.0
        );
        out.write_all(&buf)?;
        app.sessions.add_bytes(session, buf.len());
        last = Some(frame);
    }
}

fn write_update_header(buf: &mut Vec<u8>, rects: u16) {
    buf.extend([0, 0]);
    buf.extend(rects.to_be_bytes());
}

fn write_rect_header(buf: &mut Vec<u8>, r: Rect, encoding: i32) {
    for v in [r.x, r.y, r.w, r.h] {
        buf.extend((v as u16).to_be_bytes());
    }
    buf.extend(encoding.to_be_bytes());
}

/// ExtendedDesktopSize rectangle: x = reason, y = status.
fn write_ext_desktop_size(buf: &mut Vec<u8>, (w, h): (u32, u32), reason: u32, status: u32) {
    write_rect_header(buf, Rect { x: reason, y: status, w, h }, ENC_EXT_DESKTOP_SIZE);
    buf.extend([1, 0, 0, 0]);
    buf.extend(0u32.to_be_bytes());
    for v in [0u16, 0, w as u16, h as u16] {
        buf.extend(v.to_be_bytes());
    }
    buf.extend(0u32.to_be_bytes());
}

#[cfg(test)]
mod tests {
    #[test]
    fn vnc_auth_is_des_ecb() {
        let challenge = [0u8; 16];
        let r = super::vnc_response("password", &challenge);
        assert_eq!(&r[..8], &r[8..]);
        assert_ne!(r, challenge);
    }
}
