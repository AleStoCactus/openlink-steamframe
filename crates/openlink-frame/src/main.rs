//! OpenLink for Valve's Steam Frame: a native Linux (arm64) OpenXR app that streams tracking
//! and input to the PC and shows the video SteamVR renders there.
//!
//! ```text
//! openlink-frame [--server HOST[:PORT]] [--swap-eyes]
//! ```
//!
//! Environment: `OPENLINK_SERVER` (as `--server`), `OPENLINK_OPENXR_LOADER` (path to
//! `libopenxr_loader.so`), `OPENLINK_V4L2_DECODER` (`/dev/videoN`), `OPENLINK_EGL_SURFACELESS=1`,
//! `OPENLINK_MODEL` (the name the PC shows), `RUST_LOG`.

#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod decoder;
#[cfg(target_os = "linux")]
mod egl;
#[cfg(target_os = "linux")]
mod render;
#[cfg(target_os = "linux")]
mod v4l2;
#[cfg(target_os = "linux")]
mod xr_egl;

#[cfg(target_os = "linux")]
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    std::panic::set_hook(Box::new(|p| log::error!("panic: {p}")));

    let mut options = app::Options { server: None, swap_eyes: false };
    let mut server = std::env::var("OPENLINK_SERVER").ok();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server" => server = args.next(),
            "--swap-eyes" => options.swap_eyes = true,
            "-h" | "--help" => {
                println!("usage: openlink-frame [--server HOST[:PORT]] [--swap-eyes]");
                return;
            }
            other => {
                eprintln!("unknown argument {other}; see --help");
                std::process::exit(2);
            }
        }
    }
    if let Some(s) = server.filter(|s| !s.is_empty()) {
        match app::parse_server(&s) {
            Some(addr) => options.server = Some(addr),
            None => {
                eprintln!("cannot parse server address {s:?} (want an IP, optionally with :port)");
                std::process::exit(2);
            }
        }
    }
    if let Err(e) = app::run(options) {
        log::error!("fatal: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("openlink-frame runs on the Steam Frame (Linux); there is nothing to run on this platform");
    std::process::exit(1);
}
