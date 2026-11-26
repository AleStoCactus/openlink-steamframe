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
//! backend: PipeWire on Linux, Windows still to come (see [`windows`]). Everything else
//! (packetizing, the microphone's jitter buffer) is shared in [`Bridge`].

#[cfg(target_os = "linux")]
mod pipewire;
#[cfg(windows)]
mod windows;

use crate::session::HeadsetLink;
use openlink_protocol::audio::{self, Packetizer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Shown in the system's device lists.
pub const DEVICE_DESCRIPTION: &str = "OpenLink";

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
}

impl Bridge {
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
    pub fn start(link: HeadsetLink) -> std::io::Result<Self> {
        let bridge = Arc::new(Bridge { link, speakers: Mutex::new(Packetizer::new(audio::Stream::Speakers)) });
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
