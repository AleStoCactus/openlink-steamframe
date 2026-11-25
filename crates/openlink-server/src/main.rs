//! `openlink`: the desktop app.
//!
//! Runs the headset session in the background and shows a settings/status window.
//! `openlink --headless` runs the session without a window (for servers or debugging).

mod config;
mod encoder;
mod gui;
mod session;

use config::Settings;
use session::{Session, Status};
use std::sync::{Arc, Mutex};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let headless = std::env::args().any(|a| a == "--headless");
    let settings = Arc::new(Mutex::new(Settings::load()));
    let status = Arc::new(Mutex::new(Status::default()));

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
