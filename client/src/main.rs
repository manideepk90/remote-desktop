// No console window behind the app on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use remote_desk_viewer::app::App;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("remote_desk_viewer=info")).init();
    let icon = egui::IconData { rgba: include_bytes!("../assets/icon-64.rgba").to_vec(), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Remote Desk Viewer")
            .with_app_id("remote-desk-viewer")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 420.0])
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native("Remote Desk Viewer", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
