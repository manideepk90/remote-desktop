mod app;
mod capture;
mod config;
mod desktop;
mod frame;
mod install;
mod keymap;
mod pairing;
mod proto;
mod rfb;
mod session;
mod sessions;
mod ui;

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
Remote Desk - VNC server for KDE Plasma (Wayland)

Usage: remote-desk [command]

Commands:
  serve               Run the server (default)
  settings            Open the settings page (starts the server if needed)
  set-password        Set the VNC password (reads it from the terminal)
  install [--autostart]
                      Add the app menu entry and background service for this user
  uninstall           Remove them again (settings are kept)
  snapshot <file>     Save the current screen image as PNG
  help                Show this help
";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("serve") {
        "serve" => serve(),
        "settings" => open_settings(),
        "set-password" => set_password(),
        "install" => install::install(args.iter().any(|a| a == "--autostart")),
        "uninstall" => install::uninstall(),
        // Used by packaging/install-system.sh.
        "print-desktop-entry" => {
            print!("{}", install::desktop_entry(&std::env::current_exe()?.to_string_lossy()));
            Ok(())
        }
        "print-service-unit" => {
            print!("{}", install::service_unit(&std::env::current_exe()?.to_string_lossy()));
            Ok(())
        }
        "snapshot" => snapshot(args.get(1).context("usage: remote-desk snapshot <file.png>")?),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command '{other}'\n\n{USAGE}"),
    }
}

fn serve() -> Result<()> {
    let cfg = config::load()?;
    if cfg.password.is_empty() {
        log::warn!("no VNC password set yet; run `remote-desk set-password` or use the settings page");
    }
    // Keeps the KWin access grant pointing at this binary.
    if let Err(e) = install::write_desktop_entry() {
        log::warn!("could not write the .desktop entry: {e:#}");
    }
    let app = app::App::new(cfg);
    ui::start(app.clone())?;
    rfb::serve(app);
    Ok(())
}

fn ui_reachable(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).is_ok()
}

fn open_settings() -> Result<()> {
    let port = config::load()?.ui_port;
    if !ui_reachable(port) {
        let as_service = std::process::Command::new("systemctl")
            .args(["--user", "start", ui::SERVICE])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !as_service {
            use std::os::unix::process::CommandExt;
            std::process::Command::new(std::env::current_exe()?)
                .arg("serve")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .process_group(0)
                .spawn()
                .context("could not start the server")?;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while !ui_reachable(port) {
            anyhow::ensure!(std::time::Instant::now() < deadline, "the server did not start; run `remote-desk serve` to see why");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    let token = std::fs::read_to_string(ui::token_path()).context("settings token missing")?;
    let url = format!("http://127.0.0.1:{port}/#t={}", token.trim());
    std::process::Command::new("xdg-open").arg(&url).spawn().context("xdg-open not found")?;
    Ok(())
}

fn set_password() -> Result<()> {
    eprint!("New VNC password (6-8 characters; VNC ignores anything past 8): ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let pw = line.trim_end_matches(['\r', '\n']).to_string();
    validate_password(&pw)?;
    let mut cfg = config::load()?;
    cfg.password = pw;
    config::save(&cfg)?;
    eprintln!("Password saved. A running server picks it up on the next connection after a restart.");
    Ok(())
}

pub fn validate_password(pw: &str) -> Result<()> {
    if pw.chars().count() < 6 {
        bail!("use at least 6 characters");
    }
    if pw.len() > 8 {
        bail!("VNC passwords are limited to 8 characters (the protocol silently ignores the rest)");
    }
    if !pw.is_ascii() {
        bail!("use ASCII characters only; VNC clients encode others inconsistently");
    }
    Ok(())
}

fn snapshot(out: &str) -> Result<()> {
    let cfg = config::load()?;
    let cap = capture::Capture::start(app::capture_settings(&cfg));
    let _attached = cap.attach();
    let frame = cap
        .store
        .wait_newer(0, Duration::from_secs(10))
        .with_context(|| format!("no frame captured ({})", cap.status()))?;
    let mut rgb = Vec::with_capacity(frame.data.len() / 4 * 3);
    for p in frame.data.chunks_exact(4) {
        rgb.extend([p[2], p[1], p[0]]);
    }
    let mut enc = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(out)?), frame.width, frame.height);
    enc.set_color(png::ColorType::Rgb);
    enc.write_header()?.write_image_data(&rgb)?;
    println!("saved {}x{} to {out}", frame.width, frame.height);
    Ok(())
}
