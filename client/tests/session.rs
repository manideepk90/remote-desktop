//! End-to-end tests: a scripted RFB server on localhost talks to a real `Session`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, KeyInit};
use egui::Color32;
use flate2::{Compress, Compression, FlushCompress};
use md5::{Digest, Md5};
use num_bigint::BigUint;
use remote_desk_viewer::rfb::{ConnectOptions, Credentials, Event, Session, auth};

fn read_n(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    s.read_exact(&mut b).unwrap();
    b
}

/// One client-to-server message.
fn read_message(s: &mut TcpStream) -> Vec<u8> {
    let mut m = read_n(s, 1);
    match m[0] {
        3 => m.extend(read_n(s, 9)),
        4 => m.extend(read_n(s, 7)),
        5 => m.extend(read_n(s, 5)),
        6 => {
            m.extend(read_n(s, 7));
            let len = u32::from_be_bytes(m[4..8].try_into().unwrap()) as usize;
            m.extend(read_n(s, len));
        }
        t => panic!("unexpected client message {t}"),
    }
    m
}

fn u32be(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn rect_header(x: u16, y: u16, w: u16, h: u16, enc: i32) -> Vec<u8> {
    let mut v = Vec::new();
    for n in [x, y, w, h] {
        v.extend(n.to_be_bytes());
    }
    v.extend(enc.to_be_bytes());
    v
}

fn px(r: u8, g: u8, b: u8) -> [u8; 4] {
    [r, g, b, 0]
}

/// Server side of ClientInit + ServerInit, then checks the client's setup messages.
fn init(s: &mut TcpStream, w: u16, h: u16, name: &str) {
    let shared = read_n(s, 1);
    assert_eq!(shared, [1], "viewer asks to share the desktop");
    let mut m = Vec::new();
    m.extend(w.to_be_bytes());
    m.extend(h.to_be_bytes());
    m.extend([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
    m.extend(u32be(name.len() as u32));
    m.extend(name.as_bytes());
    s.write_all(&m).unwrap();

    let pf = read_n(s, 20);
    assert_eq!(pf[0], 0, "SetPixelFormat first");
    assert_eq!(&pf[4..8], &[32, 24, 0, 1], "32bpp true colour little-endian");
    assert_eq!(&pf[14..17], &[0, 8, 16], "red in the low byte");
    let enc = read_n(s, 4);
    assert_eq!(enc[0], 2, "SetEncodings");
    let n = u16::from_be_bytes([enc[2], enc[3]]) as usize;
    let list: Vec<i32> = read_n(s, n * 4).chunks(4).map(|c| i32::from_be_bytes(c.try_into().unwrap())).collect();
    for e in [16, 5, 1, 0, -223, -308] {
        assert!(list.contains(&e), "encoding {e} requested: {list:?}");
    }
    let req = read_n(s, 10);
    assert_eq!(req[..2], [3, 0], "first update request is not incremental");
    assert_eq!(u16::from_be_bytes([req[6], req[7]]), w);
}

struct Client {
    session: Session,
    events: Vec<Event>,
}

impl Client {
    fn start(port: u16, username: &str, password: Option<&str>) -> Client {
        let session = Session::start(
            ConnectOptions {
                host: "127.0.0.1".into(),
                port,
                username: username.into(),
                password: password.map(String::from),
                shared: true,
            },
            || {},
        );
        Client { session, events: Vec::new() }
    }

    /// Waits until an event matches, keeping everything seen so far.
    fn wait_for(&mut self, what: &str, pred: impl Fn(&Event) -> bool) -> &Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(i) = self.events.iter().position(&pred) {
                return &self.events[i];
            }
            self.events.extend(self.session.events());
            assert!(Instant::now() < deadline, "timed out waiting for {what}; got {:?}", self.events);
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_fb(&self, what: &str, pred: impl Fn(&remote_desk_viewer::rfb::Framebuffer) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pred(&self.session.fb.lock().unwrap()) {
            if Instant::now() > deadline {
                let fb = self.session.fb.lock().unwrap();
                panic!(
                    "timed out waiting for {what}: {}x{} p0={:?} p1024={:?} cursor={} events={:?} {:?}",
                    fb.width,
                    fb.height,
                    fb.pixels.first(),
                    fb.pixels.get(1024),
                    fb.cursor.is_some(),
                    self.events,
                    self.session.events().collect::<Vec<_>>()
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn listener() -> (TcpListener, u16) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    (l, port)
}

#[test]
fn vnc_auth_updates_input_and_clipboard() {
    let (listener, port) = listener();
    let (to_test, from_server) = mpsc::channel::<Vec<u8>>();
    let (go, wait_for_test) = mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"RFB 003.008\n").unwrap();
        assert_eq!(read_n(&mut s, 12), b"RFB 003.008\n");
        s.write_all(&[2, 2, 30]).unwrap(); // VNC auth and ARD
        assert_eq!(read_n(&mut s, 1), [auth::VNC], "no username, so VNC auth");
        let challenge = [9u8; 16];
        s.write_all(&challenge).unwrap();
        let response = read_n(&mut s, 16);
        assert_eq!(response, auth::vnc_response("secret", &challenge));
        s.write_all(&u32be(0)).unwrap();
        init(&mut s, 64, 32, "Test Desk");

        // Update 1: raw, hextile, ZRLE, CopyRect, desktop name and a cursor.
        let mut m = vec![0, 0];
        m.extend(6u16.to_be_bytes());
        m.extend(rect_header(0, 0, 2, 1, 0));
        m.extend(px(255, 0, 0));
        m.extend(px(0, 255, 0));
        m.extend(rect_header(16, 0, 16, 16, 5));
        m.push(2); // background only
        m.extend(px(0, 0, 255));
        let mut z = Compress::new(Compression::default(), true);
        let mut tiles = vec![1, 10, 20, 30];
        let mut comp = Vec::with_capacity(1024);
        z.compress_vec(&tiles, &mut comp, FlushCompress::Sync).unwrap();
        m.extend(rect_header(32, 16, 32, 16, 16));
        m.extend(u32be(comp.len() as u32));
        m.extend(&comp);
        m.extend(rect_header(0, 16, 2, 1, 1)); // copy the raw pixels down
        m.extend(0u16.to_be_bytes());
        m.extend(0u16.to_be_bytes());
        m.extend(rect_header(0, 0, 0, 0, -307));
        m.extend(u32be(7));
        m.extend(b"Renamed");
        m.extend(rect_header(1, 1, 2, 2, -239));
        for _ in 0..4 {
            m.extend(px(255, 255, 255));
        }
        m.extend([0xc0, 0x40]);
        s.write_all(&m).unwrap();
        let req = read_n(&mut s, 10);
        assert_eq!(req[..2], [3, 1], "then incremental requests");

        // Update 2: resize, then a second ZRLE rectangle on the same zlib stream.
        wait_for_test.recv().unwrap();
        let mut m = vec![0, 0];
        m.extend(2u16.to_be_bytes());
        m.extend(rect_header(0, 0, 80, 40, -308));
        m.extend([1, 0, 0, 0]);
        m.extend([0; 16]);
        tiles = vec![1, 40, 50, 60];
        comp.clear();
        z.compress_vec(&tiles, &mut comp, FlushCompress::Sync).unwrap();
        m.extend(rect_header(70, 30, 10, 10, 16));
        m.extend(u32be(comp.len() as u32));
        m.extend(&comp);
        s.write_all(&m).unwrap();

        // The next update request and the viewer's input may arrive in either order.
        let (mut request, mut input) = (None, Vec::new());
        while request.is_none() || input.len() < 3 {
            let msg = read_message(&mut s);
            if msg[0] == 3 { request = Some(msg) } else { input.push(msg) }
        }
        let req = request.unwrap();
        assert_eq!(u16::from_be_bytes([req[6], req[7]]), 80, "requests cover the new size");
        to_test.send(input.concat()).unwrap();

        // Clipboard from the server, then hang up.
        let mut m = vec![3, 0, 0, 0];
        m.extend(u32be(5));
        m.extend(b"h\xe9llo");
        s.write_all(&m).unwrap();
        thread::sleep(Duration::from_millis(200));
    });

    let mut c = Client::start(port, "", Some("secret"));
    c.wait_for("authenticated", |e| matches!(e, Event::Authenticated { password: Some(p), .. } if p == "secret"));
    c.wait_for("connected", |e| matches!(e, Event::Connected { name } if name == "Test Desk"));
    c.wait_fb("first update", |fb| {
        fb.width == 64 && fb.pixels[16 * 64] == Color32::from_rgb(255, 0, 0) && fb.cursor.is_some()
    });
    {
        let fb = c.session.fb.lock().unwrap();
        assert_eq!(fb.pixels[0], Color32::from_rgb(255, 0, 0));
        assert_eq!(fb.pixels[1], Color32::from_rgb(0, 255, 0));
        assert_eq!(fb.pixels[16 * 64 + 1], Color32::from_rgb(0, 255, 0), "CopyRect");
        assert_eq!(fb.pixels[5 * 64 + 20], Color32::from_rgb(0, 0, 255), "Hextile");
        assert_eq!(fb.pixels[31 * 64 + 63], Color32::from_rgb(10, 20, 30), "ZRLE");
        assert_eq!(fb.name, "Renamed");
        let cursor = fb.cursor.as_ref().unwrap();
        assert_eq!((cursor.width, cursor.height, cursor.hotspot), (2, 2, (1, 1)));
        assert_eq!(cursor.pixels[1], Color32::WHITE);
        assert_eq!(cursor.pixels[2], Color32::TRANSPARENT);
    }
    go.send(()).unwrap();
    c.wait_fb("resize", |fb| fb.width == 80 && fb.pixels[39 * 80 + 79] == Color32::from_rgb(40, 50, 60));
    assert_eq!(c.session.fb.lock().unwrap().pixels[0], Color32::from_rgb(255, 0, 0), "resize keeps the image");

    c.session.key(0xffe3, true);
    c.session.pointer(1, 300, 2);
    c.session.clipboard("ok€");
    let input = from_server.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(input[..8], [4, 1, 0, 0, 0, 0, 0xff, 0xe3]);
    assert_eq!(input[8..14], [5, 1, 1, 44, 0, 2]);
    assert_eq!(input[14..], [6, 0, 0, 0, 0, 0, 0, 3, b'o', b'k', b'?']);

    c.wait_for("clipboard", |e| matches!(e, Event::Clipboard(t) if t == "héllo"));
    c.wait_for("closed", |e| matches!(e, Event::Closed { error: Some(_), auth_failed: false }));
    server.join().unwrap();
}

#[test]
fn asks_for_a_password_and_reports_wrong_ones() {
    let (listener, port) = listener();
    let server = thread::spawn(move || {
        for attempt in 0..2 {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"RFB 003.008\n").unwrap();
            read_n(&mut s, 12);
            s.write_all(&[1, 2]).unwrap();
            read_n(&mut s, 1);
            let challenge = [attempt as u8; 16];
            s.write_all(&challenge).unwrap();
            let ok = read_n(&mut s, 16) == auth::vnc_response("right", &challenge);
            if ok {
                s.write_all(&u32be(0)).unwrap();
                init(&mut s, 8, 8, "desk");
                thread::sleep(Duration::from_millis(300));
            } else {
                let reason = b"Authentication failed";
                s.write_all(&u32be(1)).unwrap();
                s.write_all(&u32be(reason.len() as u32)).unwrap();
                s.write_all(reason).unwrap();
            }
        }
    });

    let mut c = Client::start(port, "", Some("wrong"));
    let e = c.wait_for("failure", |e| matches!(e, Event::Closed { .. }));
    assert!(
        matches!(e, Event::Closed { error: Some(msg), auth_failed: true } if msg.contains("Authentication failed")),
        "{e:?}"
    );
    assert!(!c.events.iter().any(|e| matches!(e, Event::Authenticated { .. })));

    let mut c = Client::start(port, "", None);
    c.wait_for("prompt", |e| matches!(e, Event::NeedCredentials { username: false }));
    c.session.answer_credentials(Some(Credentials { username: String::new(), password: "right".into() }));
    c.wait_for("connected", |e| matches!(e, Event::Connected { .. }));
    server.join().unwrap();
}

#[test]
fn cancelling_the_prompt_ends_quietly() {
    let (listener, port) = listener();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"RFB 003.008\n").unwrap();
        read_n(&mut s, 12);
        s.write_all(&[1, 2]).unwrap();
        read_n(&mut s, 1);
        s.write_all(&[0; 16]).unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest);
        assert!(rest.is_empty(), "no response is sent after cancelling");
    });
    let mut c = Client::start(port, "", None);
    c.wait_for("prompt", |e| matches!(e, Event::NeedCredentials { .. }));
    c.session.answer_credentials(None);
    c.wait_for("closed", |e| matches!(e, Event::Closed { error: None, .. }));
    drop(c);
    server.join().unwrap();
}

#[test]
fn apple_remote_desktop_login() {
    let (listener, port) = listener();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"RFB 003.889\n").unwrap(); // what macOS says
        assert_eq!(read_n(&mut s, 12), b"RFB 003.008\n");
        s.write_all(&[2, 2, 30]).unwrap();
        assert_eq!(read_n(&mut s, 1), [auth::ARD], "a username selects ARD");

        // Any modulus works for checking the exchange; real servers use a 1024-bit prime.
        let mut prime = vec![0xc5u8; 128];
        prime[0] = 0xf3;
        let p = BigUint::from_bytes_be(&prime);
        let server_private = BigUint::from_bytes_be(&[0x5a; 64]);
        let server_public = BigUint::from(2u8).modpow(&server_private, &p).to_bytes_be();
        let mut m = vec![0, 2];
        m.extend(128u16.to_be_bytes());
        m.extend(&prime);
        m.extend(vec![0u8; 128 - server_public.len()]);
        m.extend(&server_public);
        s.write_all(&m).unwrap();

        let reply = read_n(&mut s, 128 + 128);
        let client_public = BigUint::from_bytes_be(&reply[128..]);
        let shared = client_public.modpow(&server_private, &p).to_bytes_be();
        let mut padded = vec![0u8; 128 - shared.len()];
        padded.extend(shared);
        let aes = Aes128::new_from_slice(&Md5::digest(&padded)).unwrap();
        let mut creds = reply[..128].to_vec();
        for block in creds.as_chunks_mut::<16>().0 {
            aes.decrypt_block(block.into());
        }
        assert_eq!(&creds[..6], b"alice\0");
        assert_eq!(&creds[64..73], b"mac-pass\0");
        s.write_all(&u32be(0)).unwrap();
        init(&mut s, 8, 8, "Alice's Mac");
        thread::sleep(Duration::from_millis(300));
    });
    let mut c = Client::start(port, "alice", Some("mac-pass"));
    c.wait_for("connected", |e| matches!(e, Event::Connected { name } if name == "Alice's Mac"));
    server.join().unwrap();
}

#[test]
fn rfb_3_3_server() {
    let (listener, port) = listener();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"RFB 003.003\n").unwrap();
        assert_eq!(read_n(&mut s, 12), b"RFB 003.003\n");
        s.write_all(&u32be(1)).unwrap(); // no authentication
        init(&mut s, 8, 8, "old");
        thread::sleep(Duration::from_millis(300));
    });
    let mut c = Client::start(port, "", None);
    c.wait_for("connected", |e| matches!(e, Event::Connected { name } if name == "old"));
    server.join().unwrap();
}

#[test]
fn unreachable_port_is_explained() {
    let (listener, port) = listener();
    drop(listener);
    let mut c = Client::start(port, "", None);
    let e = c.wait_for("closed", |e| matches!(e, Event::Closed { .. }));
    assert!(matches!(e, Event::Closed { error: Some(m), .. } if m.contains("refused")), "{e:?}");
}
