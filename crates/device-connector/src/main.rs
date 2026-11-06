//! Device Connector – Maus, Tastatur, Zwischenablage und Dateien zwischen
//! einem Windows-PC und einem Mac teilen.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod arrange;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod audio;
mod config;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod drives;
mod logging;
#[macro_use]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod trace;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod platform;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod service;
#[cfg(any(windows, target_os = "macos"))]
mod ui;

fn main() {
    logging::init();
    log::info!("Device Connector {} startet", env!("CARGO_PKG_VERSION"));
    let cfg = config::Config::load();

    #[cfg(windows)]
    platform::init_process();

    #[cfg(any(windows, target_os = "macos"))]
    ui::run(cfg);

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = cfg;
        eprintln!("Device Connector läuft derzeit nur unter Windows und macOS.");
    }
}
