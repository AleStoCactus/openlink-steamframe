//! Windows backend: WASAPI.
//!
//! Windows cannot create audio devices from user mode, unlike PipeWire: a new output or input
//! device in the Sound settings needs a signed kernel audio driver. So the devices are existing
//! ones, used the way other streaming software does:
//!
//! * **Speakers**: loopback capture of the *default output device*. Whatever plays there is
//!   also streamed to the headset, and the capture follows the default when it changes (as
//!   choosing "OpenLink" in the Sound settings does on Linux).
//! * **Microphone**: the headset's microphone is *played into a virtual audio cable*
//!   (VB-CABLE's "CABLE Input" by default, [`crate::config::Settings::microphone_device`]);
//!   applications record from the cable's other end ("CABLE Output") as their microphone.
//!
//! WASAPI converts formats and rates (`AUTOCONVERTPCM`); we always see 48 kHz s16, as with
//! PipeWire, and the rest is [`super::Bridge`].
//!
//! A bundled virtual audio driver exposing real "OpenLink" endpoints would remove both
//! compromises; the driver could then also set `Prop_Audio_DefaultPlaybackDeviceId_String` so
//! SteamVR switches Windows to the headset's audio while it runs.

use super::{Bridge, Wanted};
use openlink_protocol::audio::{MIC_CHANNELS, SAMPLE_RATE, SPEAKER_CHANNELS};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::PWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
    WAVE_FORMAT_PCM,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};

/// How often the devices are checked against what the session wants (and the default output).
const POLL: Duration = Duration::from_millis(200);
/// How often samples are moved. `timeBeginPeriod(1)` makes sleeps this short accurate.
const TICK: Duration = Duration::from_millis(2);
/// WASAPI buffer sizes, in 100 ns units: room for scheduling hiccups.
const BUFFER_HNS: i64 = 50 * 10_000;
/// Microphone audio queued in the cable: enough to ride out a late tick, little enough to add
/// no noticeable latency.
const MIC_FILL: Duration = Duration::from_millis(20);

type WinResult<T> = windows::core::Result<T>;

fn format(channels: usize) -> WAVEFORMATEX {
    let block = (channels * 2) as u16;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: channels as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Loopback capture of one output device.
struct Speakers {
    device_id: String,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// When the last samples arrived; loopback delivers nothing while nothing plays.
    last_data: Option<Instant>,
}

impl Speakers {
    fn open(device: &IMMDevice, device_id: String) -> WinResult<Self> {
        // SAFETY: COM calls on live interfaces with valid arguments.
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let fmt = format(SPEAKER_CHANNELS);
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_HNS,
                0,
                &fmt,
                None,
            )?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self { device_id, client, capture, last_data: None })
        }
    }

    /// Moves what was captured to the headset.
    fn pump(&mut self, bridge: &Bridge) -> WinResult<()> {
        // SAFETY: buffers are used between GetBuffer and ReleaseBuffer only, with the sizes
        // WASAPI reported.
        unsafe {
            loop {
                if self.capture.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize * SPEAKER_CHANNELS;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    bridge.play(std::iter::repeat_n(0i16, n));
                } else {
                    let samples = std::slice::from_raw_parts(data.cast::<i16>(), n);
                    bridge.play(samples.iter().copied());
                }
                self.capture.ReleaseBuffer(frames)?;
                self.last_data = Some(Instant::now());
            }
        }
        // Nothing playing: drop a partly filled packet rather than send it late with the next.
        if self.last_data.is_some_and(|t| t.elapsed() > Duration::from_millis(20)) {
            bridge.play_paused();
            self.last_data = None;
        }
        Ok(())
    }
}

impl Drop for Speakers {
    fn drop(&mut self) {
        // SAFETY: a started client.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// The headset's microphone, played into an output device (a virtual cable).
struct Microphone {
    name: String,
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_frames: u32,
    samples: Vec<i16>,
}

impl Microphone {
    fn open(device: &IMMDevice, name: String) -> WinResult<Self> {
        // SAFETY: COM calls on live interfaces with valid arguments.
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let fmt = format(MIC_CHANNELS);
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_HNS,
                0,
                &fmt,
                None,
            )?;
            let render: IAudioRenderClient = client.GetService()?;
            let buffer_frames = client.GetBufferSize()?;
            client.Start()?;
            Ok(Self { name, client, render, buffer_frames, samples: Vec::new() })
        }
    }

    /// Keeps about [`MIC_FILL`] of the headset's microphone queued in the device.
    fn pump(&mut self, bridge: &Bridge) -> WinResult<()> {
        let target = (SAMPLE_RATE as u64 * MIC_FILL.as_millis() as u64 / 1000) as u32;
        // SAFETY: the buffer is written between GetBuffer and ReleaseBuffer only, within the
        // frame count we asked for.
        unsafe {
            let padding = self.client.GetCurrentPadding()?;
            let frames = target.min(self.buffer_frames).saturating_sub(padding);
            if frames == 0 {
                return Ok(());
            }
            self.samples.resize(frames as usize * MIC_CHANNELS, 0);
            bridge.record(&mut self.samples);
            let data = self.render.GetBuffer(frames)?;
            std::ptr::copy_nonoverlapping(self.samples.as_ptr(), data.cast::<i16>(), self.samples.len());
            self.render.ReleaseBuffer(frames, 0)?;
        }
        Ok(())
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        // SAFETY: a started client.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

#[derive(Default)]
struct Devices {
    speakers: Option<Speakers>,
    microphone: Option<Microphone>,
    /// The last attempt failed (and was reported); not retried until what is wanted, the
    /// microphone device setting or the default output changes.
    failed: Option<(Wanted, String, String)>,
}

/// Keeps the timer resolution at 1 ms while audio streams (restored on drop).
struct FineTimer(bool);

impl FineTimer {
    fn set(&mut self, on: bool) {
        if on != self.0 {
            // SAFETY: balanced begin/end calls.
            unsafe {
                if on {
                    timeBeginPeriod(1);
                } else {
                    timeEndPeriod(1);
                }
            }
            self.0 = on;
        }
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        self.set(false);
    }
}

pub fn run(bridge: &Arc<Bridge>, stop: &Arc<AtomicBool>) -> Result<(), String> {
    // SAFETY: COM for this thread; every COM object below lives on it.
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| format!("COM: {e}"))?;
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("audio devices: {e}"))?
    };
    log::info!("audio: using WASAPI");
    let mut devices = Devices::default();
    let mut timer = FineTimer(false);
    let mut last_poll: Option<Instant> = None;
    while !stop.load(Ordering::Acquire) {
        if last_poll.is_none_or(|t| t.elapsed() >= POLL) {
            last_poll = Some(Instant::now());
            update(bridge, &enumerator, &mut devices);
            timer.set(devices.speakers.is_some() || devices.microphone.is_some());
        }
        let mut error = None;
        if let Some(s) = &mut devices.speakers {
            if let Err(e) = s.pump(bridge) {
                error = Some(format!("output device capture stopped: {e}"));
                devices.speakers = None;
            }
        }
        if let Some(m) = &mut devices.microphone {
            if let Err(e) = m.pump(bridge) {
                error = Some(format!("microphone playback into \"{}\" stopped: {e}", m.name));
                devices.microphone = None;
            }
        }
        if let Some(e) = error {
            // A device went away (unplugged, disabled, the default changed): the next poll
            // opens it again or reports why it cannot.
            log::warn!("audio: {e}");
            last_poll = None;
        }
        std::thread::sleep(if devices.speakers.is_some() || devices.microphone.is_some() { TICK } else { POLL });
    }
    Ok(())
}

fn device_id(device: &IMMDevice) -> WinResult<String> {
    // SAFETY: GetId returns a CoTaskMem string we free.
    unsafe {
        let id: PWSTR = device.GetId()?;
        let s = id.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id.0 as *const _));
        Ok(s)
    }
}

fn friendly_name(device: &IMMDevice) -> String {
    // SAFETY: plain property read.
    unsafe {
        device
            .OpenPropertyStore(STGM_READ)
            .and_then(|p| p.GetValue(&PKEY_Device_FriendlyName))
            .map(|v| v.to_string())
            .unwrap_or_default()
    }
}

/// The active output device whose name contains `wanted` (case-insensitive).
fn find_output(enumerator: &IMMDeviceEnumerator, wanted: &str) -> WinResult<Option<(IMMDevice, String)>> {
    let wanted = wanted.to_lowercase();
    // SAFETY: enumeration of active endpoints.
    unsafe {
        let all = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..all.GetCount()? {
            let device = all.Item(i)?;
            let name = friendly_name(&device);
            if name.to_lowercase().contains(&wanted) {
                return Ok(Some((device, name)));
            }
        }
    }
    Ok(None)
}

/// Opens and closes devices to match what the session wants.
fn update(bridge: &Bridge, enumerator: &IMMDeviceEnumerator, d: &mut Devices) {
    let wanted = bridge.wanted();
    let mic_setting = bridge.microphone_device();
    // SAFETY: plain COM call.
    let default = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .ok()
        .and_then(|dev| Some((device_id(&dev).ok()?, dev)));
    let default_id = default.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();

    // Follow the default output device, as a device chosen in the Sound settings would.
    if d.speakers.as_ref().is_some_and(|s| s.device_id != default_id) {
        log::info!("audio: default output device changed");
        d.speakers = None;
    }
    if d.microphone.as_ref().is_some_and(|m| !m.name.to_lowercase().contains(&mic_setting.to_lowercase())) {
        d.microphone = None;
    }
    let have = Wanted { speakers: d.speakers.is_some(), microphone: d.microphone.is_some() };
    let key = (wanted, mic_setting.clone(), default_id.clone());
    if have == wanted || d.failed.as_ref() == Some(&key) {
        return;
    }

    let mut errors = Vec::new();
    if wanted.speakers != have.speakers {
        d.speakers = None;
        if wanted.speakers {
            match &default {
                Some((id, dev)) => match Speakers::open(dev, id.clone()) {
                    Ok(s) => {
                        log::info!("audio: streaming the default output device \"{}\" to the headset", friendly_name(dev));
                        d.speakers = Some(s);
                    }
                    Err(e) => errors.push(format!("cannot capture the default output device: {e}")),
                },
                None => errors.push("there is no default output device to stream".to_string()),
            }
        }
    }
    if wanted.microphone != have.microphone {
        d.microphone = None;
        if wanted.microphone {
            match find_output(enumerator, &mic_setting) {
                Ok(Some((dev, name))) => {
                    if device_id(&dev).is_ok_and(|id| id == default_id) && wanted.speakers {
                        // It would loop the microphone back into the headset's speakers.
                        errors.push(format!(
                            "\"{name}\" is the default output device; the headset microphone needs a separate one"
                        ));
                    } else {
                        match Microphone::open(&dev, name.clone()) {
                            Ok(m) => {
                                log::info!("audio: headset microphone plays into \"{name}\"");
                                bridge.record_started();
                                d.microphone = Some(m);
                            }
                            Err(e) => errors.push(format!("cannot play the microphone into \"{name}\": {e}")),
                        }
                    }
                }
                Ok(None) => errors.push(format!(
                    "no output device named like \"{mic_setting}\" for the headset microphone: install a virtual \
                     audio cable (e.g. VB-CABLE) or choose one in the settings"
                )),
                Err(e) => errors.push(format!("cannot list audio devices: {e}")),
            }
        }
    }
    let now = Wanted { speakers: d.speakers.is_some(), microphone: d.microphone.is_some() };
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    if let Some(e) = &error {
        log::error!("audio: {e}");
    }
    d.failed = error.is_some().then_some(key);
    bridge.set_status(now, error);
}
