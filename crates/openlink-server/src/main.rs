//! `openlink`: the desktop app.
//!
//! Runs the headset session in the background and shows a settings/status window.
//! `openlink --headless` runs the session without a window (for servers or debugging).
//! While a headset is connected, its audio reaches the PC (see [`audio`]).
//! `openlink --dump-video PATH` also writes every encoded frame to PATH as raw H.265.

mod adapter;
mod audio;
mod config;
mod encoder;
mod gui;
mod session;
#[cfg(any(target_os = "linux", windows))]
mod video;

use config::Settings;
use session::{Session, Status};
use std::sync::{Arc, Mutex};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--headless");
    #[cfg(any(target_os = "linux", windows))]
    if let Some(i) = args.iter().position(|a| a == "--encode-selftest") {
        if let Err(e) = video::selftest(&args[i + 1..]) {
            log::error!("selftest: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(any(target_os = "linux", windows))]
    if let Some(i) = args.iter().position(|a| a == "--convert-selftest") {
        if let Err(e) = video::convert_selftest(&args[i + 1..]) {
            log::error!("convert selftest: {e}");
            std::process::exit(1);
        }
        return;
    }
    let settings = Arc::new(Mutex::new(Settings::load()));
    let status = Arc::new(Mutex::new(Status::default()));
    adapter::spawn_monitor(status.clone(), settings.clone());

    let session = match Session::start(status.clone(), settings.clone()) {
        Ok(s) => Some(s),
        Err(e) => {
            let msg =
                format!("cannot open network ports: {e} (is another OpenLink instance running?)");
            log::error!("{msg}");
            status.lock().unwrap().last_error = Some(msg);
            None
        }
    };

    #[cfg(any(target_os = "linux", windows))]
    let _video = {
        let dump = args
            .iter()
            .position(|a| a == "--dump-video")
            .and_then(|i| args.get(i + 1))
            .map(std::path::PathBuf::from);
        let sink = session.as_ref().and_then(|s| s.link().ok());
        match video::VideoService::start(status.clone(), settings.clone(), sink, dump) {
            Ok(v) => Some(v),
            Err(e) => {
                log::error!("cannot listen for the driver's video link: {e}");
                None
            }
        }
    };

    let _audio = session.as_ref().and_then(|s| s.link().ok()).and_then(|link| {
        audio::AudioService::start(link, settings.clone()).map_err(|e| log::error!("cannot start audio: {e}")).ok()
    });

    if headless {
        if session.is_none() {
            std::process::exit(1);
        }
        loop {
            std::thread::park();
        }
    }

    if let Err(e) = gui::run(status, settings) {
        log::error!("GUI failed: {e}");
    }
    drop(session);
}
