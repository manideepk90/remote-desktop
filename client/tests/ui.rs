//! Renders the app headlessly and saves screenshots of each screen. Needs a GPU
//! (or a software Vulkan/GL driver) and a VNC server, so it only runs on request:
//!
//!     RDV_TEST_SERVER=127.0.0.1:15901 RDV_TEST_PASSWORD=secret RDV_SNAPSHOT_DIR=/tmp/shots \
//!         cargo test --test ui -- --ignored --nocapture

use std::time::{Duration, Instant};

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use remote_desk_viewer::app::App;

fn save(h: &mut Harness<'_, App>, name: &str) {
    let dir = std::env::var("RDV_SNAPSHOT_DIR").unwrap_or_else(|_| "target/ui-snapshots".into());
    std::fs::create_dir_all(&dir).unwrap();
    let path = format!("{dir}/{name}.png");
    h.render().expect("render").save(&path).unwrap();
    eprintln!("saved {path}");
}

fn settle(h: &mut Harness<'_, App>, what: &str, done: impl Fn(&Harness<'_, App>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(h) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    h.run_steps(4);
}

#[test]
#[ignore]
fn screenshots() {
    let server = std::env::var("RDV_TEST_SERVER").expect("RDV_TEST_SERVER=host:port");
    let password = std::env::var("RDV_TEST_PASSWORD").unwrap_or_default();
    let config = std::env::temp_dir().join(format!("rdv-ui-{}", std::process::id()));
    // Keep saved connections away from the real config. Nothing here touches the keyring.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };

    let mut h = Harness::builder().with_size([1100.0, 720.0]).wgpu().build_eframe(|cc| App::new(cc));
    h.run_steps(3);
    save(&mut h, "1-home-empty");

    h.get_all_by_role(Role::TextInput).next().unwrap().type_text(&server);
    h.get_by_role(Role::PasswordInput).focus();
    h.run_steps(1);
    h.get_by_role(Role::PasswordInput).type_text(&password);
    h.get_by_label_contains("Remember password").click();
    h.run_steps(2);
    save(&mut h, "2-form-filled");

    h.get_by_label("Connect").click();
    settle(&mut h, "connected", |h| h.query_by_label("Disconnect").is_some());
    std::thread::sleep(Duration::from_millis(800));
    h.run_steps(4);
    save(&mut h, "3-session");

    h.get_by_label("1:1").click();
    h.run_steps(4);
    save(&mut h, "4-session-actual-size");

    h.get_by_label("Disconnect").click();
    settle(&mut h, "home", |h| h.query_by_label("Connect").is_some());
    save(&mut h, "5-home-saved");

    // No password this time, so the server's request opens the sign-in dialog.
    h.get_by_label("Connect").click();
    settle(&mut h, "prompt", |h| h.query_by_label("Sign in").is_some());
    save(&mut h, "6-password-prompt");
    h.key_press(egui::Key::Escape);
    settle(&mut h, "home again", |h| h.query_by_label("Connect").is_some());

    h.ctx.set_theme(egui::Theme::Light);
    h.run_steps(3);
    save(&mut h, "7-home-light");

    let _ = std::fs::remove_dir_all(config);
}
