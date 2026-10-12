//! Connects without a window, optionally types some text, and saves the screen
//! as a PPM image. Handy for checking a server from a terminal.
//!
//!     RDV_PASSWORD=secret cargo run --example snapshot -- host:5901 out.ppm "echo hi\n"
//!
//! `RDV_USERNAME` sets the username for account logins.

use std::io::Write;
use std::time::{Duration, Instant};

use remote_desk_viewer::keys::char_keysym;
use remote_desk_viewer::rfb::{ConnectOptions, Event, Session, parse_address};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (Some(addr), Some(out)) = (args.get(1), args.get(2)) else {
        anyhow::bail!("usage: snapshot HOST[:PORT] OUT.ppm [TEXT-TO-TYPE]");
    };
    let (host, port) = parse_address(addr, 5900).map_err(anyhow::Error::msg)?;
    let session = Session::start(
        ConnectOptions {
            host,
            port,
            username: std::env::var("RDV_USERNAME").unwrap_or_default(),
            password: std::env::var("RDV_PASSWORD").ok(),
            shared: true,
        },
        || {},
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    'connect: loop {
        for e in session.events() {
            eprintln!("{e:?}");
            match e {
                Event::Connected { .. } => break 'connect,
                Event::Closed { error, .. } => anyhow::bail!("closed: {error:?}"),
                Event::NeedCredentials { .. } => anyhow::bail!("set RDV_PASSWORD (and RDV_USERNAME)"),
                _ => {}
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(800));
    if let Some(text) = args.get(3) {
        let (w, h) = {
            let fb = session.fb.lock().unwrap();
            (fb.width, fb.height)
        };
        // Click the middle of the screen to focus whatever is there, then type.
        session.pointer(0, (w / 2) as u16, (h / 2) as u16);
        session.pointer(1, (w / 2) as u16, (h / 2) as u16);
        session.pointer(0, (w / 2) as u16, (h / 2) as u16);
        for c in text.replace("\\n", "\n").chars() {
            let sym = char_keysym(c);
            session.key(sym, true);
            session.key(sym, false);
        }
        std::thread::sleep(Duration::from_millis(1200));
    }
    for e in session.events() {
        eprintln!("{e:?}");
    }
    let fb = session.fb.lock().unwrap();
    let mut f = std::io::BufWriter::new(std::fs::File::create(out)?);
    write!(f, "P6\n{} {}\n255\n", fb.width, fb.height)?;
    for p in &fb.pixels {
        f.write_all(&[p.r(), p.g(), p.b()])?;
    }
    eprintln!("saved {}x{} \"{}\" to {out}", fb.width, fb.height, fb.name);
    Ok(())
}
