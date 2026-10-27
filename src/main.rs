//! Rust Radio — a GTK4 internet radio player for radio-browser.info.

mod api;
mod icons;
mod model;
mod mpris;
mod net_safety;
mod persistence;
mod player;
mod playlist;
mod station_cache;
mod ui;

use adw::prelude::*;

fn main() -> gtk::glib::ExitCode {
    if let Err(err) = gstreamer::init() {
        eprintln!("Failed to initialize GStreamer: {}", err);
        return gtk::glib::ExitCode::FAILURE;
    }
    let application = adw::Application::builder()
        .application_id("io.github.antoxa78.RustRadio")
        .build();
    application.connect_activate(|app| {
        // Launching the app again just raises the existing window instead
        // of building a second, independent player.
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        ui::build(app);
    });
    application.run()
}
