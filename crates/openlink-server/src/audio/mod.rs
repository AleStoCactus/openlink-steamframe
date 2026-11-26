//! Headset audio on the PC.
//!
//! While a headset is connected the PC gets two audio devices, both named "OpenLink":
//!
//! * an **output** device: whatever plays on it is streamed to the headset's speakers;
//! * an **input** device: the headset's microphone.
//!
//! They appear in the system sound settings like any plugged-in headset, and disappear again
//! when the headset leaves, so sound falls back to the previous device. The desktop
//! remembers the choice by device name, so picking OpenLink once sticks across connections.
//!
//! The platform half (creating the devices, moving samples in and out of them) lives in a
//! backend: PipeWire on Linux, WASAPI on Windows. Everything else (packetizing, the
//! microphone's jitter buffer) is shared in [`Bridge`].
//!
//! Windows cannot create devices from user mode, so there the speakers are the default output
//! device (captured in loopback) and the microphone plays into a virtual audio cable; see
//! [`windows`].

#[cfg(target_os = "linux")]
mod pipewire;
#[cfg(windows)]
mod windows;

use crate::config::Settings;
use crate::session::HeadsetLink;
use openlink_protocol::audio::{self, Packetizer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Shown in the system's device lists.
#[cfg(target_os = "linux")]
pub const DEVICE_DESCRIPTION: &str = "OpenLink";

/// What the speakers setting does, for the GUI.
pub const SPEAKERS_HINT: &str = if cfg!(windows) {
    "What plays on the default output device is also heard in the headset"
} else {
    "Adds an \"OpenLink\" output device; what plays on it is heard in the headset"
};
/// What the microphone setting does, for the GUI.
pub const MICROPHONE_HINT: &str = if cfg!(windows) {
    "Plays the headset's microphone into a virtual audio cable (the device below); applications \
     record from the cable's other end"
} else {
    "Adds an \"OpenLink\" input device carrying the headset's microphone"
};

/// Which devices should exist right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Wanted {
    pub speakers: bool,
    pub microphone: bool,
}

/// What a backend talks to: the headset session, with audio in the protocol's formats
/// ([`audio::SAMPLE_RATE`], interleaved s16).
pub struct Bridge {
    link: HeadsetLink,
    speakers: Mutex<Packetizer>,
    #[cfg_attr(not(windows), allow(dead_code))]
    settings: Arc<Mutex<Settings>>,
}

impl Bridge {
    /// Windows: the output device (a virtual cable) the headset's microphone plays into.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn microphone_device(&self) -> String {
        self.settings.lock().unwrap().microphone_device.clone()
    }

    /// Devices follow the connected headset and what it was told to stream.
    pub fn wanted(&self) -> Wanted {
        self.link
            .stream_config()
            .map(|c| Wanted { speakers: c.speakers, microphone: c.microphone })
            .unwrap_or_default()
    }

    /// Sends game audio ([`audio::SPEAKER_CHANNELS`] interleaved) to the headset.
    pub fn play(&self, samples: impl IntoIterator<Item = i16>) {
        let Some(addr) = self.link.headset() else { return };
        let mut peak = 0u16;
        let samples = samples.into_iter().inspect(|s| peak = peak.max(s.unsigned_abs()));
        self.speakers.lock().unwrap().push(samples, |d| {
            let _ = self.link.send(d, addr);
        });
        self.link.audio_levels().speakers(peak);
    }

    /// Game audio paused: drop a partly filled packet rather than send it late with the next.
    pub fn play_paused(&self) {
        self.speakers.lock().unwrap().flush();
    }

    /// Fills `out` ([`audio::MIC_CHANNELS`] interleaved) with microphone audio, silence where
    /// there is none.
    pub fn record(&self, out: &mut [i16]) {
        self.link.mic().lock().unwrap().pop(out);
    }

    /// Recording (re)started: drop whatever piled up while nobody listened.
    pub fn record_started(&self) {
        self.link.mic().lock().unwrap().reset();
    }

    pub fn set_status(&self, wanted: Wanted, error: Option<String>) {
        let mut st = self.link.status().lock().unwrap();
        st.audio.speakers = wanted.speakers;
        st.audio.microphone = wanted.microphone;
        st.audio.error = error;
    }
}

pub struct AudioService {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioService {
    pub fn start(link: HeadsetLink, settings: Arc<Mutex<Settings>>) -> std::io::Result<Self> {
        let bridge =
            Arc::new(Bridge { link, speakers: Mutex::new(Packetizer::new(audio::Stream::Speakers)), settings });
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new().name("audio".into()).spawn(move || {
                if let Err(e) = run(&bridge, &stop) {
                    log::error!("audio: {e}");
                    bridge.set_status(Wanted::default(), Some(e));
                }
            })?
        };
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for AudioService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Runs the platform backend until `stop`.
fn run(bridge: &Arc<Bridge>, stop: &Arc<AtomicBool>) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    return pipewire::run(bridge, stop);
    #[cfg(windows)]
    return windows::run(bridge, stop);
    #[allow(unreachable_code)]
    {
        let _ = (bridge, stop);
        Err("audio is not supported on this platform yet".into())
    }
}
