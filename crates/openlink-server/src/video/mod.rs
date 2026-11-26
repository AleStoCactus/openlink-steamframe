//! Video pipeline (Linux): compositor frames from the driver -> NV12 -> H.265.
//!
//! ```text
//!  driver (vrserver)                       server, video thread
//!  ─────────────────                       ─────────────────────────────────────────────────
//!  GBM dmabufs ──fds once──────────────►  import as VkImages (DRM format modifier)
//!  Present ──Frame{textures, bounds}───►  wait on the dmabufs' implicit write fences
//!                                          compute: both eyes -> side-by-side NV12  ──┐
//!            ◄──Release{frame}──────────  (textures free again)                       │
//!                                          encode queue: Vulkan Video H.265  ◄─────────┘
//!                                          -> Annex B bitstream -> UDP to the headset
//! ```
//!
//! Each frame goes to the headset as [`openlink_protocol::video`] datagrams, with the HMD pose
//! SteamVR rendered it with. New headsets and reported losses get an IDR.
//!
//! One frame is in flight at a time. If frames queue up while one is encoded, only the newest
//! is processed and the older ones are released straight away.

mod convert;
mod encode;
mod import;
mod vk;

use crate::config::Settings;
use crate::session::{Status, VideoSink};
use ash::vk as avk;
use import::{color_range, Imported};
use openlink_protocol::video_ipc::{self as vipc, Channel, DriverVideoMsg, Listener, ServerVideoMsg};
use openlink_protocol::{self as proto, video};
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vk::{check, Context, Result};

pub struct VideoService {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct Shared {
    status: Arc<Mutex<Status>>,
    settings: Arc<Mutex<Settings>>,
    dump: Option<PathBuf>,
    sink: Option<Arc<VideoSink>>,
}

impl VideoService {
    /// Listens for the driver. `dump`: append every encoded frame to this file (raw H.265).
    /// `sink`: where frames go (the headset session).
    pub fn start(
        status: Arc<Mutex<Status>>,
        settings: Arc<Mutex<Settings>>,
        sink: Option<VideoSink>,
        dump: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        let listener = Listener::bind()?;
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Shared { status, settings, dump, sink: sink.map(Arc::new) };
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("openlink-video".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        match listener.accept(200) {
                            Ok(Some(ch)) => {
                                log::info!("video: driver connected");
                                Connection::new(&shared).run(&ch, &stop);
                                log::info!("video: driver disconnected");
                            }
                            Ok(None) => {}
                            Err(e) => {
                                log::error!("video: accept: {e}");
                                std::thread::sleep(Duration::from_secs(1));
                            }
                        }
                    }
                })?
        };
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for VideoService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Rate frames are sent at: well above the stream's average (so a frame still leaves in a
/// couple of milliseconds) but below what Wi-Fi absorbs in one burst.
const PACING_BPS: f64 = 1.2e9;

/// Sends frames to the headset from its own thread, paced: Wi-Fi access points drop from a
/// few hundred back-to-back datagrams once their queue fills. Ends when the sender is dropped.
fn spawn_sender(sink: Option<Arc<VideoSink>>) -> std::sync::mpsc::SyncSender<(video::FrameInfo, Vec<u8>)> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<(video::FrameInfo, Vec<u8>)>(1);
    std::thread::Builder::new()
        .name("video-send".into())
        .spawn(move || {
            let mut packetizer = video::Packetizer::default();
            for (info, bitstream) in rx {
                let Some((sink, addr)) = sink.as_deref().and_then(|s| Some((s, s.headset()?))) else { continue };
                let start = Instant::now();
                let mut sent = 0usize;
                packetizer.packetize(&info, &bitstream, |d| {
                    let due = Duration::from_secs_f64(sent as f64 * 8.0 / PACING_BPS);
                    while start.elapsed() < due {
                        std::hint::spin_loop();
                    }
                    let _ = sink.send(d, addr);
                    sent += d.len();
                });
            }
        })
        .expect("spawn video sender");
    tx
}

struct Frame {
    id: u64,
    eyes: [vipc::EyeLayer; 2],
    overlay: Option<[vipc::EyeLayer; 2]>,
    hmd_pose: [[f32; 4]; 3],
    present_ns: i64,
}

/// What to encode: per-eye picture size and frame rate.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Target {
    eye_width: u32,
    eye_height: u32,
    fps: f32,
    bitrate_bps: u64,
}

impl Target {
    fn current(shared: &Shared) -> Self {
        let settings = shared.settings.lock().unwrap().clone();
        let status = shared.status.lock().unwrap();
        // Before a headset connects, encode at Quest 3's recommended size so the pipeline can
        // be tested.
        let (w, h, fps) = status
            .headset
            .as_ref()
            .map(|h| (h.info.eye_width, h.info.eye_height, h.info.current_refresh_rate))
            .unwrap_or((1680, 1760, settings.refresh_rate));
        let even = |v: f32| ((v.round() as u32) & !1).max(2);
        Self {
            eye_width: even(w as f32 * settings.resolution_scale),
            eye_height: even(h as f32 * settings.resolution_scale),
            fps,
            bitrate_bps: settings.bitrate_mbps as u64 * 1_000_000,
        }
    }

    /// Shrinks the eyes (keeping their aspect) until two side by side fit the encoder.
    fn fit(mut self, max: avk::Extent2D) -> Self {
        let scale = (max.width as f32 / (2 * self.eye_width) as f32)
            .min(max.height as f32 / self.eye_height as f32)
            .min(1.0);
        if scale < 1.0 {
            let even = |v: f32| ((v.floor() as u32) & !1).max(2);
            let (w, h) = (even(self.eye_width as f32 * scale), even(self.eye_height as f32 * scale));
            log::warn!(
                "video: {}x{} per eye does not fit the encoder's {}x{}; encoding {w}x{h} per eye",
                self.eye_width,
                self.eye_height,
                max.width,
                max.height
            );
            self.eye_width = w;
            self.eye_height = h;
        }
        self
    }
}

/// GPU objects for one encode configuration.
struct Pipeline {
    /// What was asked for, before fitting it to the encoder.
    requested: Target,
    converter: convert::Converter,
    encoder: encode::Encoder,
    pool: avk::CommandPool,
    cmd: avk::CommandBuffer,
    fence: avk::Fence,
    converted: avk::Semaphore,
    /// Receive the compositor's write fences, one per texture read (two eyes, two overlay).
    fence_sems: [avk::Semaphore; 4],
    /// Compositor write fences seen: already signalled, still pending, unavailable.
    fence_stats: (u32, u32, u32),
}

impl Pipeline {
    fn new(ctx: &Context, requested: Target) -> Result<Self> {
        let target = requested.fit(encode::max_extent(ctx)?);
        let encoder = encode::Encoder::new(
            ctx,
            &encode::EncoderConfig {
                width: 2 * target.eye_width,
                height: target.eye_height,
                fps: target.fps,
                bitrate_bps: target.bitrate_bps,
            },
        )?;
        let converter = convert::Converter::new(
            ctx,
            encoder.layout.coded_width,
            encoder.layout.coded_height,
            target.eye_width,
            target.eye_height,
        )?;
        let pool = ctx.command_pool(ctx.compute_family)?;
        Ok(Self {
            requested,
            converter,
            encoder,
            cmd: ctx.command_buffer(pool)?,
            pool,
            fence: ctx.fence()?,
            converted: ctx.semaphore()?,
            fence_sems: [ctx.semaphore()?, ctx.semaphore()?, ctx.semaphore()?, ctx.semaphore()?],
            fence_stats: (0, 0, 0),
        })
    }

    fn destroy(self, ctx: &Context) {
        // SAFETY: the device is idle (caller).
        unsafe {
            let d = &ctx.device;
            d.destroy_fence(self.fence, None);
            d.destroy_semaphore(self.converted, None);
            self.fence_sems.iter().for_each(|&s| d.destroy_semaphore(s, None));
            d.destroy_command_pool(self.pool, None);
        }
        self.converter.destroy(ctx);
        self.encoder.destroy(ctx);
    }

    /// Converts the frame on the compute queue and waits for it, so the textures can be
    /// released before encoding.
    /// `overlay`: SteamVR's own layer to draw over an application's, with its bounds.
    fn convert(
        &mut self,
        ctx: &Context,
        eyes: [&Imported; 2],
        bounds: [[f32; 4]; 2],
        overlay: Option<([&Imported; 2], [[f32; 4]; 2])>,
    ) -> Result<()> {
        let d = &ctx.device;
        // Every distinct texture this frame reads.
        let mut unique: Vec<&Imported> = Vec::new();
        for img in eyes.iter().chain(overlay.iter().flat_map(|(o, _)| o.iter())) {
            if !unique.iter().any(|u| u.handle == img.handle) {
                unique.push(img);
            }
        }
        // Wait for the compositor's rendering into the textures (implicit dmabuf fences).
        let mut waits = Vec::new();
        for (eye, sem) in unique.iter().zip(self.fence_sems) {
            let Some(sync) = eye.write_fence() else {
                self.fence_stats.2 += 1;
                continue;
            };
            // Diagnostics: is the compositor's write still in flight when we get here?
            let mut pfd = libc::pollfd { fd: sync.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: valid fd and pollfd.
            let signaled = unsafe { libc::poll(&mut pfd, 1, 0) } == 1;
            if signaled { self.fence_stats.0 += 1 } else { self.fence_stats.1 += 1 }
            let info = avk::ImportSemaphoreFdInfoKHR::default()
                .semaphore(sem)
                .flags(avk::SemaphoreImportFlags::TEMPORARY)
                .handle_type(avk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
                .fd(sync.as_raw_fd());
            // SAFETY: valid semaphore and sync file; Vulkan owns the fd on success.
            match unsafe { ctx.ext_sem_fd.import_semaphore_fd(&info) } {
                Ok(()) => {
                    let _ = sync.into_raw_fd();
                    waits.push(sem);
                }
                Err(e) => log::warn!("video: cannot import compositor fence: {e:?}"),
            }
        }

        let ownership = |img: &Imported, acquire: bool| {
            let (src_q, dst_q) = if acquire {
                (avk::QUEUE_FAMILY_FOREIGN_EXT, ctx.compute_family)
            } else {
                (ctx.compute_family, avk::QUEUE_FAMILY_FOREIGN_EXT)
            };
            let b = avk::ImageMemoryBarrier2::default()
                .image(img.image)
                .subresource_range(color_range())
                .src_queue_family_index(src_q)
                .dst_queue_family_index(dst_q);
            if acquire {
                b.old_layout(avk::ImageLayout::GENERAL)
                    .new_layout(avk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .dst_stage_mask(avk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(avk::AccessFlags2::SHADER_SAMPLED_READ)
            } else {
                b.old_layout(avk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .new_layout(avk::ImageLayout::GENERAL)
                    .src_stage_mask(avk::PipelineStageFlags2::COMPUTE_SHADER)
                    .src_access_mask(avk::AccessFlags2::SHADER_SAMPLED_READ)
            }
        };
        let mut acquire: Vec<_> = unique.iter().map(|i| ownership(i, true)).collect();
        acquire.push(
            avk::ImageMemoryBarrier2::default()
                .image(self.encoder.src)
                .subresource_range(color_range())
                .old_layout(avk::ImageLayout::UNDEFINED)
                .new_layout(avk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_stage_mask(avk::PipelineStageFlags2::COPY)
                .dst_access_mask(avk::AccessFlags2::TRANSFER_WRITE),
        );
        let release: Vec<_> = unique.iter().map(|i| ownership(i, false)).collect();

        self.converter.bind_eyes(ctx, eyes[0].view, eyes[1].view, overlay.map(|(o, _)| (o[0].view, o[1].view)));
        // SAFETY: our command buffer is idle (we wait for its fence every frame).
        unsafe {
            check(
                d.begin_command_buffer(self.cmd, &avk::CommandBufferBeginInfo::default().flags(avk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)),
                "vkBeginCommandBuffer",
            )?;
            d.cmd_pipeline_barrier2(self.cmd, &avk::DependencyInfo::default().image_memory_barriers(&acquire));
            self.converter.record(ctx, self.cmd, bounds, overlay.map(|(_, b)| b), self.encoder.src);
            d.cmd_pipeline_barrier2(self.cmd, &avk::DependencyInfo::default().image_memory_barriers(&release));
            check(d.end_command_buffer(self.cmd), "vkEndCommandBuffer")?;
            let stages = vec![avk::PipelineStageFlags::COMPUTE_SHADER; waits.len()];
            let cmds = [self.cmd];
            let signal = [self.converted];
            let submit = avk::SubmitInfo::default()
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&cmds)
                .signal_semaphores(&signal);
            check(d.queue_submit(ctx.compute_queue, &[submit], self.fence), "vkQueueSubmit (convert)")?;
            ctx.wait_fence(self.fence, "convert")?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    skipped: u32,
    bytes: u64,
    convert: Duration,
    encode: Duration,
    send_errors: u32,
}

struct Connection<'a> {
    shared: &'a Shared,
    ctx: Option<Context>,
    imports: HashMap<u64, Imported>,
    pipeline: Option<Pipeline>,
    dump: Option<std::fs::File>,
    /// `<dump>.poses`: one line per dumped frame.
    dump_poses: Option<std::fs::File>,
    stats_frames_total: u64,
    /// Frames for the sender thread, which paces them onto the network.
    sender: std::sync::mpsc::SyncSender<(video::FrameInfo, Vec<u8>)>,
    stats: Stats,
    failed: bool,
    eye_layout: Option<String>,
}

impl<'a> Connection<'a> {
    fn new(shared: &'a Shared) -> Self {
        let dump = shared.dump.as_ref().and_then(|p| match std::fs::File::create(p) {
            Ok(f) => {
                log::info!("video: writing H.265 to {}", p.display());
                Some(f)
            }
            Err(e) => {
                log::error!("video: cannot create {}: {e}", p.display());
                None
            }
        });
        let dump_poses = shared
            .dump
            .as_ref()
            .filter(|_| dump.is_some())
            .and_then(|p| std::fs::File::create(p.with_extension("poses")).ok());
        Self {
            shared,
            dump_poses,
            stats_frames_total: 0,
            ctx: None,
            imports: HashMap::new(),
            pipeline: None,
            dump,
            sender: spawn_sender(shared.sink.clone()),
            stats: Stats::default(),
            failed: false,
            eye_layout: None,
        }
    }

    fn run(mut self, ch: &Channel, stop: &AtomicBool) {
        let mut buf = vec![0u8; vipc::MAX_MESSAGE];
        let release = |id| {
            let _ = ch.send(&proto::encode(&ServerVideoMsg::Release { frame_id: id }), &[]);
        };
        while !stop.load(Ordering::Acquire) {
            // Wait for something, then drain what is queued so only the newest frame is encoded.
            let mut newest: Option<Frame> = None;
            let mut timeout = 100;
            loop {
                let (n, fds) = match ch.recv(&mut buf, timeout) {
                    Ok(Some(m)) => m,
                    Ok(None) => break,
                    Err(e) => {
                        if e.kind() != std::io::ErrorKind::UnexpectedEof {
                            log::warn!("video: {e}");
                        }
                        if let Some(f) = newest {
                            release(f.id);
                        }
                        self.shutdown();
                        return;
                    }
                };
                timeout = 0;
                match proto::decode::<DriverVideoMsg>(&buf[..n]) {
                    Ok(DriverVideoMsg::Hello { driver_version }) => log::info!("video: driver {driver_version}"),
                    Ok(DriverVideoMsg::TextureSet(set)) => self.import_set(&set, fds),
                    Ok(DriverVideoMsg::TexturesDestroyed { handles }) => self.forget(&handles),
                    Ok(DriverVideoMsg::Frame { frame_id, eyes, overlay, hmd_pose, present_ns }) => {
                        if let Some(old) = newest.replace(Frame { id: frame_id, eyes, overlay, hmd_pose, present_ns }) {
                            self.stats.skipped += 1;
                            release(old.id);
                        }
                    }
                    Err(e) => log::warn!("video: bad message from driver: {e}"),
                }
            }
            if let Some(frame) = newest {
                self.process(&frame, &release);
            }
        }
        self.shutdown();
    }

    fn import_set(&mut self, set: &vipc::TextureSet, fds: Vec<OwnedFd>) {
        let planes: usize = set.textures.iter().map(|t| t.planes.len()).sum();
        if fds.len() != planes {
            log::error!("video: texture set with {planes} planes came with {} fds", fds.len());
            return;
        }
        if self.ctx.is_none() && !self.failed {
            match Context::new(set.render_node) {
                Ok(c) => self.ctx = Some(c),
                Err(e) => {
                    log::error!("video: no Vulkan encoder: {e}");
                    self.failed = true;
                }
            }
        }
        let Some(ctx) = &self.ctx else { return };
        let mut fds = fds.into_iter();
        for tex in &set.textures {
            let tex_fds: Vec<OwnedFd> = fds.by_ref().take(tex.planes.len()).collect();
            match Imported::new(ctx, set, tex, tex_fds) {
                Ok(img) => {
                    self.imports.insert(tex.handle, img);
                }
                Err(e) => log::error!("video: cannot import texture {:#x}: {e}", tex.handle),
            }
        }
        log::info!(
            "video: imported {}x{} {} textures (modifier {:#x}, {} plane(s))",
            set.width,
            set.height,
            String::from_utf8_lossy(&set.drm_format.to_le_bytes()),
            set.modifier,
            set.textures.first().map_or(0, |t| t.planes.len())
        );
    }

    fn forget(&mut self, handles: &[u64]) {
        let Some(ctx) = &self.ctx else { return };
        ctx.wait_idle();
        for h in handles {
            if let Some(img) = self.imports.remove(h) {
                img.destroy(ctx);
            }
        }
    }

    fn process(&mut self, frame: &Frame, release: &dyn Fn(u64)) {
        let result = self.try_process(frame, release);
        if let Err(e) = result {
            log::error!("video: frame {}: {e}", frame.id);
            if self.ctx.as_ref().is_some_and(Context::is_lost) {
                log::error!("video: GPU hung or lost; encoding disabled until the driver reconnects");
                self.abandon();
                return;
            }
            // Rebuild on the next frame.
            if let (Some(ctx), Some(p)) = (&self.ctx, self.pipeline.take()) {
                ctx.wait_idle();
                p.destroy(ctx);
            }
        }
    }

    fn try_process(&mut self, frame: &Frame, release: &dyn Fn(u64)) -> Result<()> {
        let Some(ctx) = &self.ctx else {
            release(frame.id);
            return Ok(());
        };
        let (Some(left), Some(right)) = (self.imports.get(&frame.eyes[0].texture), self.imports.get(&frame.eyes[1].texture)) else {
            release(frame.id);
            return Ok(());
        };
        // What SteamVR submits per eye, logged when it changes (texture sharing, sizes, bounds).
        let layout = format!(
            "left {}x{} bounds {:?}, right {}x{} bounds {:?}, {}",
            left.width,
            left.height,
            frame.eyes[0].bounds,
            right.width,
            right.height,
            frame.eyes[1].bounds,
            if left.handle == right.handle { "one shared texture" } else { "separate textures" }
        );
        if self.eye_layout.as_deref() != Some(&layout) {
            log::info!("video: eyes: {layout}");
            self.eye_layout = Some(layout);
        }
        let target = Target::current(self.shared);
        if self.pipeline.as_ref().is_some_and(|p| p.requested != target) {
            ctx.wait_idle();
            self.pipeline.take().unwrap().destroy(ctx);
        }
        if self.pipeline.is_none() {
            self.pipeline = Some(Pipeline::new(ctx, target)?);
        }
        let p = self.pipeline.as_mut().unwrap();

        let t0 = Instant::now();
        // The overlay is drawn only if both its textures are known.
        let overlay = frame.overlay.and_then(|o| {
            let (l, r) = (self.imports.get(&o[0].texture)?, self.imports.get(&o[1].texture)?);
            Some(([l, r], [o[0].bounds, o[1].bounds]))
        });
        let converted = p.convert(ctx, [left, right], [frame.eyes[0].bounds, frame.eyes[1].bounds], overlay);
        release(frame.id);
        converted?;
        let t1 = Instant::now();
        let sink = self.shared.sink.as_deref();
        if sink.is_some_and(VideoSink::take_idr_request) {
            p.encoder.request_idr();
        }
        let (bitstream, idr) = p.encoder.encode(ctx, p.converted)?;
        let t2 = Instant::now();

        if let Some(f) = &mut self.dump {
            if let Err(e) = f.write_all(&bitstream) {
                log::error!("video: dump write failed: {e}");
                self.dump = None;
            }
            // Beside the stream: each frame's render pose, for checking labels against content.
            if let Some(poses) = &mut self.dump_poses {
                let p = proto::Pose::from_matrix34(&frame.hmd_pose);
                let _ = writeln!(
                    poses,
                    "{} {} {} {:?} {:?}",
                    self.stats_frames_total, frame.present_ns, idr as u8, p.orientation, p.position
                );
            }
            self.stats_frames_total += 1;
        }
        let bytes = bitstream.len();
        if sink.is_some_and(|s| s.headset().is_some()) {
            let info = video::FrameInfo {
                idr,
                render_pose: proto::Pose::from_matrix34(&frame.hmd_pose),
                present_ns: frame.present_ns,
            };
            if self.sender.try_send((info, bitstream)).is_err() {
                self.stats.send_errors += 1; // the sender is still busy with the previous frame
            }
        }
        let s = &mut self.stats;
        let since = *s.since.get_or_insert(t0);
        s.frames += 1;
        s.bytes += bytes as u64;
        s.convert += t1 - t0;
        s.encode += t2 - t1;
        let elapsed = t2 - since;
        if elapsed >= Duration::from_secs(5) {
            let n = s.frames.max(1);
            if let Some(p) = &mut self.pipeline {
                let (sig, pend, none) = std::mem::take(&mut p.fence_stats);
                log::info!("video: compositor write fences: {sig} already signalled, {pend} pending, {none} unavailable");
            }
            log::info!(
                "video: {:.1} fps, {:.1} Mbit/s, {:.0} KB/frame, convert {:.2} ms, encode {:.2} ms, {} skipped, {} send errors",
                s.frames as f64 / elapsed.as_secs_f64(),
                s.bytes as f64 * 8.0 / elapsed.as_secs_f64() / 1e6,
                s.bytes as f64 / n as f64 / 1e3,
                s.convert.as_secs_f64() * 1e3 / n as f64,
                s.encode.as_secs_f64() * 1e3 / n as f64,
                s.skipped,
                s.send_errors
            );
            *s = Stats::default();
        }
        Ok(())
    }

    /// Drops the GPU state without touching a lost device: its objects are leaked, as
    /// destroying them while the GPU may still use them is undefined.
    fn abandon(&mut self) {
        std::mem::forget(self.pipeline.take());
        std::mem::forget(std::mem::take(&mut self.imports));
        self.ctx = None;
        self.failed = true;
    }

    fn shutdown(&mut self) {
        if self.ctx.as_ref().is_some_and(Context::is_lost) {
            self.abandon();
            return;
        }
        if let Some(ctx) = &self.ctx {
            ctx.wait_idle();
            if let Some(p) = self.pipeline.take() {
                p.destroy(ctx);
            }
            for (_, img) in self.imports.drain() {
                img.destroy(ctx);
            }
        }
        self.ctx = None;
    }
}

/// `openlink --encode-selftest WxH FRAMES [OUT [SOURCE.yuv]]`: drives the encoder alone (no
/// SteamVR) to debug Vulkan Video without the rest of the pipeline. The source is a scrolling
/// test pattern, identical in the left and right halves (so per-half quality can be compared);
/// SOURCE.yuv receives it as raw NV12 for `ffmpeg -lavfi psnr`.
pub fn selftest(args: &[String]) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let size = args.first().ok_or("usage: --encode-selftest WxH FRAMES [OUT]")?;
    let (w, h) = size.split_once('x').ok_or("size must be WxH")?;
    let (width, height): (u32, u32) = (w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?);
    let frames: u32 = args.get(1).and_then(|n| n.parse().ok()).unwrap_or(300);
    let mut out = args.get(2).map(std::fs::File::create).transpose().map_err(|e| e.to_string())?;
    let mut source_out = args.get(3).map(std::fs::File::create).transpose().map_err(|e| e.to_string())?;
    let rdev = std::fs::metadata("/dev/dri/renderD128").map_err(|e| e.to_string())?.rdev();
    let ctx = Context::new(rdev)?;
    let mut enc = encode::Encoder::new(&ctx, &encode::EncoderConfig { width, height, fps: 90.0, bitrate_bps: 200_000_000 })?;
    let d = &ctx.device;
    let pool = ctx.command_pool(ctx.compute_family)?;
    let cmd = ctx.command_buffer(pool)?;
    let fence = ctx.fence()?;
    let sem = ctx.semaphore()?;
    // Staging buffer for the pattern, coded size (the encoder's source image is coded size).
    let (cw, ch) = (enc.layout.coded_width as usize, enc.layout.coded_height as usize);
    let nv12_len = cw * ch * 3 / 2;
    // SAFETY: valid create info.
    let staging = check(
        unsafe {
            d.create_buffer(
                &avk::BufferCreateInfo::default().size(nv12_len as u64).usage(avk::BufferUsageFlags::TRANSFER_SRC),
                None,
            )
        },
        "vkCreateBuffer (staging)",
    )?;
    let staging_mem =
        ctx.bind_buffer_memory(staging, avk::MemoryPropertyFlags::HOST_VISIBLE | avk::MemoryPropertyFlags::HOST_COHERENT)?;
    // SAFETY: host-visible memory we just bound, mapped whole.
    let staging_ptr = check(unsafe { d.map_memory(staging_mem, 0, avk::WHOLE_SIZE, avk::MemoryMapFlags::empty()) }, "vkMapMemory")?
        as *mut u8;
    let mut pattern = vec![0u8; nv12_len];
    let barrier = avk::ImageMemoryBarrier2::default()
        .image(enc.src)
        .subresource_range(color_range())
        .old_layout(avk::ImageLayout::UNDEFINED)
        .new_layout(avk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .dst_stage_mask(avk::PipelineStageFlags2::COPY)
        .dst_access_mask(avk::AccessFlags2::TRANSFER_WRITE);
    let t0 = Instant::now();
    let mut bytes = 0usize;
    let result = (|| -> Result<()> {
        for i in 0..frames {
            fill_pattern(&mut pattern, cw, ch, width as usize / 2, i);
            if let Some(f) = &mut source_out {
                f.write_all(&pattern).map_err(|e| e.to_string())?;
            }
            // SAFETY: our idle command buffer; valid handles; staging is not in use (we wait for
            // every submission).
            unsafe {
                std::ptr::copy_nonoverlapping(pattern.as_ptr(), staging_ptr, nv12_len);
                check(d.begin_command_buffer(cmd, &avk::CommandBufferBeginInfo::default()), "vkBeginCommandBuffer")?;
                d.cmd_pipeline_barrier2(cmd, &avk::DependencyInfo::default().image_memory_barriers(std::slice::from_ref(&barrier)));
                let plane = |aspect, offset: usize, w: usize, h: usize| avk::BufferImageCopy {
                    buffer_offset: offset as u64,
                    buffer_row_length: w as u32,
                    buffer_image_height: h as u32,
                    image_subresource: avk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: 0, base_array_layer: 0, layer_count: 1 },
                    image_offset: avk::Offset3D::default(),
                    image_extent: avk::Extent3D { width: w as u32, height: h as u32, depth: 1 },
                };
                d.cmd_copy_buffer_to_image(
                    cmd,
                    staging,
                    enc.src,
                    avk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[plane(avk::ImageAspectFlags::PLANE_0, 0, cw, ch), plane(avk::ImageAspectFlags::PLANE_1, cw * ch, cw / 2, ch / 2)],
                );
                check(d.end_command_buffer(cmd), "vkEndCommandBuffer")?;
                let (cmds, signal) = ([cmd], [sem]);
                let submit = avk::SubmitInfo::default().command_buffers(&cmds).signal_semaphores(&signal);
                check(d.queue_submit(ctx.compute_queue, &[submit], fence), "vkQueueSubmit (selftest)")?;
            }
            ctx.wait_fence(fence, "selftest prep")?;
            let (bs, _) = enc.encode(&ctx, sem).map_err(|e| format!("frame {i}: {e}"))?;
            bytes += bs.len();
            if let Some(f) = &mut out {
                f.write_all(&bs).map_err(|e| e.to_string())?;
            }
            if i < 3 || (i + 1) % 100 == 0 {
                log::info!("selftest: frame {i}: {} bytes", bs.len());
            }
        }
        Ok(())
    })();
    let secs = t0.elapsed().as_secs_f64();
    log::info!("selftest: {frames} frames in {secs:.2} s ({:.1} fps), {:.1} MB", frames as f64 / secs, bytes as f64 / 1e6);
    if ctx.is_lost() {
        std::mem::forget(enc);
        return result;
    }
    // SAFETY: idle device; our objects.
    unsafe {
        let _ = d.device_wait_idle();
        d.destroy_semaphore(sem, None);
        d.destroy_buffer(staging, None);
        d.free_memory(staging_mem, None);
        d.destroy_fence(fence, None);
        d.destroy_command_pool(pool, None);
    }
    enc.destroy(&ctx);
    result
}

/// A detailed, moving NV12 test pattern whose left and right halves (each `half` pixels wide)
/// are identical: fine noise, edges and gradients scrolling diagonally.
fn fill_pattern(buf: &mut [u8], w: usize, h: usize, half: usize, frame: u32) {
    let hash = |x: u32, y: u32| {
        let mut v = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
        v ^= v >> 15;
        v = v.wrapping_mul(0x2C1B_3C6D);
        (v ^ (v >> 12)) as u8
    };
    let (dx, dy) = (frame * 3, frame * 2);
    for y in 0..h {
        for x in 0..w {
            let lx = (x % half) as u32 + dx;
            let ly = y as u32 + dy;
            let checker = if ((lx / 32) + (ly / 32)) % 2 == 0 { 40 } else { 0 };
            let ramp = ((lx % 256) / 4) as u8;
            let noise = hash(lx / 2, ly / 2) / 4;
            buf[y * w + x] = 60u8.saturating_add(checker).saturating_add(ramp).saturating_add(noise);
        }
    }
    let uv = &mut buf[w * h..];
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let lx = ((2 * x) % half) as u32 + dx;
            let ly = (2 * y) as u32 + dy;
            uv[y * w + 2 * x] = 128u8.wrapping_add(((lx / 64) % 4 * 12) as u8);
            uv[y * w + 2 * x + 1] = 128u8.wrapping_sub(((ly / 64) % 4 * 12) as u8);
        }
    }
}
