//! PipeWire backend: the devices are two `pw_stream`s with a device media class.
//!
//! A stream with `media.class = Audio/Sink` is an output device other applications play into
//! (what `pw-loopback` and `module-null-sink` create), and one with `Audio/Source` is an input
//! device they record from. PipeWire converts formats and rates; we always see 48 kHz s16.
//! Works with PulseAudio applications too, through `pipewire-pulse`.

use super::{Bridge, Wanted, DEVICE_DESCRIPTION};
use openlink_protocol::audio::{MIC_CHANNELS, SAMPLE_RATE, SPEAKER_CHANNELS};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How often the devices are checked against what the session wants.
const POLL: Duration = Duration::from_millis(200);
/// The quantum we ask the graph for: 10 ms.
const LATENCY: &str = "480/48000";

struct Device {
    stream: StreamRc,
    _listener: StreamListener<()>,
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.stream.disconnect();
    }
}

#[derive(Default)]
struct Devices {
    speakers: Option<Device>,
    microphone: Option<Device>,
    /// The last creation failed (and was reported); not retried until what is wanted changes.
    failed: Option<Wanted>,
}

pub fn run(bridge: &Arc<Bridge>, stop: &Arc<AtomicBool>) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("PipeWire context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("cannot connect to PipeWire (is it running?): {e}"))?;
    let _core_listener = core
        .add_listener_local()
        .error(|id, _seq, res, message| log::warn!("PipeWire error on object {id}: {message} ({res})"))
        .register();
    log::info!("audio: connected to PipeWire");

    let devices = Rc::new(RefCell::new(Devices::default()));
    let timer = mainloop.loop_().add_timer({
        let (bridge, stop, core, devices) = (bridge.clone(), stop.clone(), core.clone(), devices.clone());
        let mainloop = mainloop.downgrade();
        move |_| {
            if stop.load(Ordering::Acquire) {
                devices.replace(Devices::default());
                if let Some(l) = mainloop.upgrade() {
                    l.quit();
                }
                return;
            }
            update(&bridge, &core, &mut devices.borrow_mut());
        }
    });
    timer
        .update_timer(Some(Duration::from_millis(1)), Some(POLL))
        .into_sync_result()
        .map_err(|e| format!("PipeWire timer: {e}"))?;
    mainloop.run();
    Ok(())
}

/// Creates and removes devices to match what the session wants.
fn update(bridge: &Arc<Bridge>, core: &pw::core::CoreRc, d: &mut Devices) {
    let wanted = bridge.wanted();
    let have = Wanted { speakers: d.speakers.is_some(), microphone: d.microphone.is_some() };
    if have == wanted || d.failed == Some(wanted) {
        return;
    }
    let mut error = None;
    if wanted.speakers != have.speakers {
        d.speakers = None;
        if wanted.speakers {
            match speakers(bridge, core) {
                Ok(dev) => d.speakers = Some(dev),
                Err(e) => error = Some(format!("cannot create the OpenLink output device: {e}")),
            }
        }
    }
    if wanted.microphone != have.microphone {
        d.microphone = None;
        if wanted.microphone {
            match microphone(bridge, core) {
                Ok(dev) => d.microphone = Some(dev),
                Err(e) => error = Some(format!("cannot create the OpenLink microphone: {e}")),
            }
        }
    }
    let now = Wanted { speakers: d.speakers.is_some(), microphone: d.microphone.is_some() };
    if now != have {
        log::info!("audio: OpenLink output device {}, microphone {}", on_off(now.speakers), on_off(now.microphone));
    }
    if let Some(e) = &error {
        log::error!("audio: {e}");
    }
    d.failed = error.is_some().then_some(wanted);
    bridge.set_status(now, error);
}

fn on_off(b: bool) -> &'static str {
    if b {
        "present"
    } else {
        "removed"
    }
}

/// The output device: applications play into it, we send what they play to the headset.
fn speakers(bridge: &Arc<Bridge>, core: &pw::core::CoreRc) -> Result<Device, pw::Error> {
    let stream = StreamRc::new(
        core.clone(),
        "openlink-speakers",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => "openlink_speakers",
            *pw::keys::NODE_DESCRIPTION => DEVICE_DESCRIPTION,
            *pw::keys::NODE_LATENCY => LATENCY,
            "device.icon-name" => "audio-headset",
            "device.form-factor" => "headset",
        },
    )?;
    let listener = stream
        .add_local_listener::<()>()
        .state_changed({
            let bridge = bridge.clone();
            move |_, _, old, new| {
                log::debug!("audio: output device {old:?} -> {new:?}");
                if !matches!(new, StreamState::Streaming) {
                    bridge.play_paused();
                }
            }
        })
        .process({
            let bridge = bridge.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let Some(d) = buffer.datas_mut().first_mut() else { return };
                let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
                if let Some(bytes) = d.data() {
                    let end = (offset + size).min(bytes.len());
                    let bytes = &bytes[offset.min(end)..end];
                    bridge.play(bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)));
                }
            }
        })
        .register()?;
    let format = format_pod(SPEAKER_CHANNELS as u32, &[spa::sys::SPA_AUDIO_CHANNEL_FL, spa::sys::SPA_AUDIO_CHANNEL_FR]);
    let mut params = [spa::pod::Pod::from_bytes(&format).expect("valid format pod")];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    Ok(Device { stream, _listener: listener })
}

/// The input device: we fill it with the headset's microphone, applications record from it.
fn microphone(bridge: &Arc<Bridge>, core: &pw::core::CoreRc) -> Result<Device, pw::Error> {
    let stream = StreamRc::new(
        core.clone(),
        "openlink-microphone",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::NODE_NAME => "openlink_microphone",
            *pw::keys::NODE_DESCRIPTION => DEVICE_DESCRIPTION,
            *pw::keys::NODE_LATENCY => LATENCY,
            "device.icon-name" => "audio-headset",
            "device.form-factor" => "headset",
        },
    )?;
    let mut samples: Vec<i16> = Vec::new();
    let listener = stream
        .add_local_listener::<()>()
        .state_changed({
            let bridge = bridge.clone();
            move |_, _, old, new| {
                log::debug!("audio: microphone {old:?} -> {new:?}");
                if matches!(new, StreamState::Streaming) {
                    bridge.record_started();
                }
            }
        })
        .process({
            let bridge = bridge.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let requested = buffer.requested() as usize;
                let Some(d) = buffer.datas_mut().first_mut() else { return };
                let stride = 2 * MIC_CHANNELS;
                let frames = match d.data() {
                    Some(bytes) => {
                        let room = bytes.len() / stride;
                        let frames = if requested > 0 { requested.min(room) } else { room };
                        samples.resize(frames * MIC_CHANNELS, 0);
                        bridge.record(&mut samples);
                        for (out, s) in bytes.as_chunks_mut::<2>().0.iter_mut().zip(&samples) {
                            *out = s.to_le_bytes();
                        }
                        frames
                    }
                    None => 0,
                };
                let chunk = d.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as i32;
                *chunk.size_mut() = (frames * stride) as u32;
            }
        })
        .register()?;
    let format = format_pod(MIC_CHANNELS as u32, &[spa::sys::SPA_AUDIO_CHANNEL_MONO]);
    let mut params = [spa::pod::Pod::from_bytes(&format).expect("valid format pod")];
    stream.connect(
        spa::utils::Direction::Output,
        None,
        StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    Ok(Device { stream, _listener: listener })
}

/// An `EnumFormat` param for 48 kHz interleaved s16 with these channels.
fn format_pod(channels: u32, positions: &[u32]) -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(channels);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[..positions.len()].copy_from_slice(positions);
    info.set_position(position);
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .expect("serialise format pod")
    .0
    .into_inner()
}
