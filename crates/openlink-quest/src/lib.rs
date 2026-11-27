//! OpenLink for Meta Quest: an OpenXR app that streams tracking, input and the microphone to
//! the PC, and shows the video SteamVR renders and plays its audio.
#![cfg(target_os = "android")]

mod app;
mod audio;
mod battery;
mod decoder;
mod egl;
mod render;

use android_activity::AndroidApp;

#[no_mangle]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("OpenLink"),
    );
    std::panic::set_hook(Box::new(|p| log::error!("panic: {p}")));
    if let Err(e) = app::run(&app) {
        log::error!("fatal: {e}");
    }
}
