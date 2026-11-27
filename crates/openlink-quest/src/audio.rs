//! Headset audio: plays the PC's game audio and records the microphone for it, through AAudio.
//!
//! Streams open once the server asks for them ([`StreamConfig::speakers`] /
//! [`StreamConfig::microphone`]) and are reopened if the system closes them (an audio device
//! change, such as headphones plugged in). The microphone also needs the `RECORD_AUDIO`
//! permission, requested at startup; until it is granted the PC's microphone stays silent.
//!
//! [`StreamConfig::speakers`]: openlink_protocol::StreamConfig::speakers
//! [`StreamConfig::microphone`]: openlink_protocol::StreamConfig::microphone

use android_activity::AndroidApp;
use jni::objects::{JObject, JValue};
use jni::{jni_sig, jni_str};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode, AudioSharingMode,
    AudioStream, AudioStreamBuilder, AudioUsage,
};
use openlink_client::{ClientState, MicSender};
use openlink_protocol::audio::{JitterBuffer, MIC_CHANNELS, SAMPLE_RATE, SPEAKER_CHANNELS};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RECORD_AUDIO: &str = "android.permission.RECORD_AUDIO";
const POLL: Duration = Duration::from_millis(250);
/// Wait this long before reopening a stream that failed.
const RETRY: Duration = Duration::from_secs(2);

/// Asks for the microphone permission unless the app already has it. Android shows its own
/// dialog; the answer is picked up by polling [`microphone_allowed`].
pub fn request_microphone_permission(app: &AndroidApp) {
    if microphone_allowed(app) {
        return;
    }
    let r = with_activity(app, |env, activity| {
        let name = env.new_string(RECORD_AUDIO)?;
        let names = env.new_object_array(1, jni_str!("java/lang/String"), JObject::null())?;
        names.set_element(env, 0, &name)?;
        env.call_method(
            activity,
            jni_str!("requestPermissions"),
            jni_sig!("([Ljava/lang/String;I)V"),
            &[JValue::Object(&names), JValue::Int(1)],
        )?;
        Ok(())
    });
    match r {
        Ok(()) => log::info!("asked for the microphone permission"),
        Err(e) => log::warn!("cannot ask for the microphone permission: {e}"),
    }
}

pub fn microphone_allowed(app: &AndroidApp) -> bool {
    with_activity(app, |env, activity| {
        let name = env.new_string(RECORD_AUDIO)?;
        let r = env.call_method(
            activity,
            jni_str!("checkSelfPermission"),
            jni_sig!("(Ljava/lang/String;)I"),
            &[JValue::Object(&name)],
        )?;
        Ok(r.i()? == 0) // PackageManager.PERMISSION_GRANTED
    })
    .unwrap_or(false)
}

pub(crate) fn with_activity<T>(
    app: &AndroidApp,
    f: impl FnOnce(&mut jni::Env, &JObject) -> jni::errors::Result<T>,
) -> jni::errors::Result<T> {
    // SAFETY: android-activity's JavaVM pointer is valid for the life of the process.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    vm.attach_current_thread(|env| {
        // SAFETY: the activity is a global reference held by NativeActivity for its lifetime;
        // a JObject does not delete the reference it wraps.
        let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
        f(env, &activity)
    })
}

/// An open AAudio stream and whether the system has given up on it.
struct Open {
    stream: AudioStream,
    failed: Arc<AtomicBool>,
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = self.stream.request_stop();
    }
}

/// Opens and closes the streams to follow the session, until `stop`.
pub fn spawn(
    app: AndroidApp,
    speakers: Arc<Mutex<JitterBuffer>>,
    mic: MicSender,
    state: Arc<Mutex<ClientState>>,
    stop: Arc<AtomicBool>,
) {
    let mic = Arc::new(Mutex::new(mic));
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let (mut output, mut input): (Option<Open>, Option<Open>) = (None, None);
            let (mut output_retry, mut input_retry) = (Instant::now(), Instant::now());
            let mut allowed = (false, Instant::now() - RETRY);
            let mut last_log = Instant::now();
            while !stop.load(Ordering::Acquire) {
                let cfg = state.lock().unwrap().welcome.clone();
                let want_out = cfg.as_ref().is_some_and(|c| c.speakers);
                let mut want_in = cfg.as_ref().is_some_and(|c| c.microphone);
                if want_in && !allowed.0 && allowed.1.elapsed() >= RETRY {
                    allowed = (microphone_allowed(&app), Instant::now());
                    if !allowed.0 {
                        log::info!("microphone permission not granted (yet); the PC gets silence");
                    }
                }
                want_in &= allowed.0;

                follow(&mut output, want_out, &mut output_retry, "speakers", || open_output(speakers.clone()));
                follow(&mut input, want_in, &mut input_retry, "microphone", || open_input(mic.clone()));

                if output.is_some() && last_log.elapsed() >= Duration::from_secs(10) {
                    let jb = speakers.lock().unwrap();
                    let s = jb.stats;
                    log::info!(
                        "audio: buffered {:.1} ms; {} packets, {} lost, {} late, {} underruns, {} dropped / {} repeated frames; {} output xruns",
                        jb.level() as f32 * 1e3 / SAMPLE_RATE as f32,
                        s.packets,
                        s.lost,
                        s.late,
                        s.underruns,
                        s.dropped_frames,
                        s.repeated_frames,
                        output.as_ref().map_or(0, |o| o.stream.x_run_count()),
                    );
                    last_log = Instant::now();
                }
                std::thread::sleep(POLL);
            }
        })
        .expect("spawn audio thread");
}

/// Opens or closes one stream to match `want`, and replaces one that failed.
fn follow(
    open: &mut Option<Open>,
    want: bool,
    retry_at: &mut Instant,
    what: &str,
    create: impl FnOnce() -> ndk::audio::Result<Open>,
) {
    if open.as_ref().is_some_and(|o| o.failed.load(Ordering::Acquire)) {
        log::warn!("audio: {what} stream lost; reopening");
        *open = None;
    }
    if !want {
        if open.take().is_some() {
            log::info!("audio: {what} closed");
        }
        return;
    }
    if open.is_some() || Instant::now() < *retry_at {
        return;
    }
    match create() {
        Ok(o) => {
            log::info!(
                "audio: {what} open, {} Hz, {} frames per burst, {} buffered",
                o.stream.sample_rate(),
                o.stream.frames_per_burst(),
                o.stream.buffer_size_in_frames()
            );
            *open = Some(o);
        }
        Err(e) => {
            log::error!("audio: cannot open {what}: {e}");
            *retry_at = Instant::now() + RETRY;
        }
    }
}

fn builder(direction: AudioDirection, channels: usize, failed: &Arc<AtomicBool>) -> ndk::audio::Result<AudioStreamBuilder> {
    let failed = failed.clone();
    Ok(AudioStreamBuilder::new()?
        .direction(direction)
        .sample_rate(SAMPLE_RATE as i32)
        .channel_count(channels as i32)
        .format(AudioFormat::PCM_I16)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .sharing_mode(AudioSharingMode::Shared)
        .error_callback(Box::new(move |_, e| {
            log::warn!("audio stream error: {e}");
            failed.store(true, Ordering::Release);
        })))
}

fn open_output(speakers: Arc<Mutex<JitterBuffer>>) -> ndk::audio::Result<Open> {
    let failed = Arc::new(AtomicBool::new(false));
    let stream = builder(AudioDirection::Output, SPEAKER_CHANNELS, &failed)?
        .usage(AudioUsage::Game)
        .data_callback(Box::new(move |_, data, frames| {
            // SAFETY: AAudio hands us `frames` frames of interleaved i16 in our channel count.
            let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<i16>(), frames as usize * SPEAKER_CHANNELS) };
            // Held only for a copy, by the network thread for a packet's worth.
            speakers.lock().unwrap().pop(out);
            AudioCallbackResult::Continue
        }))
        .open_stream()?;
    stream.request_start()?;
    Ok(Open { stream, failed })
}

fn open_input(mic: Arc<Mutex<MicSender>>) -> ndk::audio::Result<Open> {
    let failed = Arc::new(AtomicBool::new(false));
    let stream = builder(AudioDirection::Input, MIC_CHANNELS, &failed)?
        // Voice processing: echo cancellation keeps the game audio out of the microphone.
        .input_preset(AudioInputPreset::VoiceCommunication)
        .data_callback(Box::new(move |_, data, frames| {
            // SAFETY: as above, for the input direction.
            let samples = unsafe { std::slice::from_raw_parts(data.cast::<i16>(), frames as usize * MIC_CHANNELS) };
            // A non-blocking UDP send per 5 ms packet.
            mic.lock().unwrap().send(samples.iter().copied());
            AudioCallbackResult::Continue
        }))
        .open_stream()?;
    stream.request_start()?;
    Ok(Open { stream, failed })
}
