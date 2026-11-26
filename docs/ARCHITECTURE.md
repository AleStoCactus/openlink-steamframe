# OpenLink architecture

OpenLink streams SteamVR to standalone headsets (Quest 2/3/3S/Pro first) over Wi-Fi or USB.
The aim is a headset that behaves like a native SteamVR HMD:

- **Tracking** that feels like Link / Steam Link / Lighthouse: no overshooting "standalone-style"
  controller prediction.
- **Image quality and latency** at Virtual Desktop's level: zero-copy hardware encode with
  H.264, HEVC and AV1 on NVIDIA, AMD and Intel.
- **Windows and Linux** as equal targets: everything but texture sharing, sync, the
  driver link transport and audio devices is shared code.
- **Open source** (GPL-3.0). A new codebase, not derived from ALVR.

## Components

```
 Quest (Android, OpenXR)                PC
 ┌──────────────────────────┐          ┌────────────────────────────────┐        ┌──────────────────────────────┐
 │ openlink-client          │   UDP    │ openlink-server  (`openlink`)  │  UDP   │ openlink-driver              │
 │  tracking thread  ───────┼─────────►│  session: clock sync, relay ───┼───────►│  (driver_openlink.so/.dll    │
 │  (500 Hz, sample @ now)  │          │                                │loopback│   inside vrserver)           │
 │  clock-ping responder ◄──┼─────────►│  GUI (egui), settings          │◄───────┼─ heartbeats, haptics         │
 │  decoder + reprojection ◄┼──video───┤  encoder (NVENC/AMF/VAAPI/…)  ◄┼─frames──┤  HMD + 2× oculus_touch       │
 └──────────────────────────┘          └────────────────────────────────┘        └──────────────────────────────┘
```

| Crate | What it is |
|---|---|
| `openlink-protocol` | Message types, datagram framing, PC monotonic clock, headset↔PC clock sync |
| `openlink-tracking` | Pose math and the prediction policy (headset sample → `DriverPose_t`) |
| `openlink-driver` | SteamVR driver in pure Rust (OpenVR C++ vtables written by hand, layout-tested) |
| `openlink-server` | Desktop app `openlink`: session, relay, settings, GUI, encoder, audio devices |
| `openlink-client` | Headset client core + `mock-headset` for testing the PC side without a headset |

The driver is deliberately thin. It runs inside `vrserver`, so a bug there takes SteamVR down,
and it has no network access to the headset. Everything with real failure modes (sockets to the
headset, encoder, UI) lives in the server process, which can restart without restarting SteamVR.

## Tracking and prediction: what decides whether swings feel right

### Why other streamers' controllers "flop"

A standalone OpenXR app calls `xrLocateSpace(…, predictedDisplayTime)`. The Quest runtime then
extrapolates controller poses to that time, which is tuned for about 15 ms ahead. Streamers that
reuse this model ask for a pose at "now + encode + network + decode + display", roughly 40–80 ms.
The runtime's extrapolator is not built for that range, so every fast swing overshoots and then
snaps back. That is the floppy-saber feel.

### What OpenLink does (the Lighthouse / Link model)

1. **Sample at now, never predict on the headset.** A dedicated thread calls `xrLocateSpace` with
   `time = now` for the HMD and both grips at 500 Hz. It sends pose, `XrSpaceVelocity` (linear and
   angular) and the sample timestamp.
2. **Put the sample on the PC timeline.** NTP-style ping/pong runs over the same socket. The
   estimator (`protocol::clock`) keeps the median offset of the lowest-RTT quarter of a sliding
   window. This makes it robust to Wi-Fi spikes, which are asymmetric. It steps during warm-up
   and only slews afterwards, so timestamps never jump mid-session.
3. **Let SteamVR predict.** The driver reports the measured state with
   `poseTimeOffset = sample_time − now` (negative: "this pose is N ms old"). SteamVR then predicts
   to its own photon time with its own model, exactly as it does for Index controllers. Poses go
   to `TrackedDevicePoseUpdated` from the link thread the moment they arrive, not on the next
   `RunFrame`.
4. **Stale data holds still.** If a sample is more than 100 ms old (a Wi-Fi stall), velocities
   are zeroed so SteamVR holds the pose instead of extrapolating it across the room.
5. **Head error is fixed on the headset.** Each frame carries the pose SteamVR rendered it with.
   The client reprojects from that pose to the actual display pose (Link and VD do the same).

### Controllers are real `oculus_touch` devices

Controllers use `Prop_ControllerType = oculus_touch` and `{oculus}/input/touch_profile.json`,
with the Quest render models. Every game's shipped Touch bindings and grip offsets therefore
apply unchanged, which matters for Beat Saber's saber angle. Those offsets are relative to the
Touch **raw** pose. SteamVR's render models give the exact position of the OpenXR grip relative
to raw (the `openxr_grip` component: `[±0.007, −0.00183, 0.10195]`, rotated 20.6° about X,
identical for Quest 2, Touch Plus and Touch Pro). The driver converts with
`world_from_raw = world_from_grip · inverse(raw_from_grip)`. The lever arm is included in the
linear velocity (`v + ω × r`).

## Video pipeline

| Stage | Windows | Linux |
|---|---|---|
| Frame capture | `IVRDriverDirectModeComponent` → D3D11 textures with legacy shared handles, created by the driver | `IVRDriverDirectModeComponent` → GBM dmabufs (compositor) or SteamVR-allocated images shared as dmabufs (applications) |
| Driver → server | loopback TCP (`video_ipc`); handles inside the messages; a shared `ID3D11Fence` per frame | abstract-namespace `SOCK_SEQPACKET`; fds as `SCM_RIGHTS`; implicit dmabuf fences |
| Import | D3D11 opens the textures and copies each frame into NT-handle textures Vulkan imports | Vulkan imports the dmabufs directly |
| Colour convert | compute shader to NV12 (Vulkan) | same |
| Encode | Vulkan Video H.265, QP per frame chosen by the server; NVENC/AMF planned | Vulkan Video H.265, CBR; VA-API/NVENC planned |
| Transport | UDP, slice-level packetisation; FEC and adaptive bitrate planned | same |
| Decode | `AMediaCodec` in low-latency mode on the Quest | same |
| Display | OpenXR swapchain plus reprojection with the render pose | same |

Encode must be zero-copy: the texture SteamVR hands the driver is encoded on the same GPU,
without a round trip through system memory.

**Windows capture.** The driver creates every swap texture (the compositor's and each
application's) as a D3D11 texture with `D3D11_RESOURCE_MISC_SHARED`, on the adapter it names in
`Prop_GraphicsAdapterLuid_Uint64` so SteamVR renders on the same GPU. On Windows a
`SharedTextureHandle_t` is that texture's legacy shared handle, which is global, so the server
opens the same textures by the handles the driver announces. Vulkan implementations do not
reliably import legacy handles, so the server's D3D11 device copies each frame into textures of
its own with NT handles, which Vulkan imports as `D3D11_TEXTURE` memory (measured on an RX 6800:
0.9 ms for copy and convert together, two 2628x2736 eyes). D3D11 has no implicit fences: in `Present`
the driver acquires the keyed mutex of SteamVR's sync texture (the compositor holds it until its
work is submitted) and signals a shared `ID3D11Fence` to the frame's id; the server duplicates
that fence out of vrserver (`DuplicateHandle`, so vrserver must not run elevated), its copy
waits for the value on the GPU, and Vulkan waits for the copy's own fence imported as a timeline
semaphore. Windows has no `SOCK_SEQPACKET`, so the link is loopback TCP with length-prefixed
messages and a writer thread whose bounded queue gives `try_send` the same never-block
behaviour.

**AMD's Windows Vulkan Video driver** (RX 6800, driver 32.0.21045) differs from RADV in three ways
the encoder works around, all `cfg(windows)`: a session reset submitted on its own leaves every
later encode completing without output (the feedback query never becomes available), so the
reset goes into the first frame's coding scope; CBR and VBR are accepted but ignored (byte-
identical streams at 20 and 200 Mbit/s), so the encoder runs at a constant QP that the server
sets per frame from each frame's size and a VBV-like budget (`SoftRateControl`); and it writes
SAO syntax into every slice header even when the SPS has SAO off, so decoders misparsed every
slice after it (FFmpeg: `alignment_bit_equal_to_one=0`; the Quest 3 rejected the IDR and showed
nothing), and the SPS now enables SAO.

AV1 encode is a hardware feature: RTX 40+, RX 7000+, Arc / Core Ultra. On older GPUs (e.g. an
RX 6800) the GUI offers H.264/HEVC only. No software can change that.

**Foveated encoding** (`openlink_protocol::foveation`). The converter can squeeze each eye's
edges before encoding, and the Quest renderer stretches them back when it draws the picture into
the (full-size) eye swapchain. Each axis is warped separately: a band around the lens's optical
axis (where the FOV's tangents are 0, so off center on the Quest's asymmetric FOV) keeps one
encoded pixel per pixel, and beyond it the size of an encoded pixel grows linearly to about 4
pixels at the image edge, so resolution has no visible step at the band. The warp is defined in
whole pixels: the band starts on an even pixel in both the full and the encoded picture, and
each side's curve is fitted to its own (whole) full and encoded length, which keeps both
directions in closed form (a quadratic and its inverse). That alignment matters: a first version
kept the band in fractions of the image, so every encoded pixel sat a fraction of a pixel off
the displayed one, and the headset's bilinear sampling blended neighbours everywhere; the center
came out visibly softer than unfoveated at every setting. At a 50% band the Quest 3 encodes
1444x1546 per eye instead of 2064x2208, about half the pixels, so less encode time, and the
bitrate goes mostly to the center. Because the warp is separable, the full-resolution region is
a rectangle and the corners are squeezed on both axes.

On top of that, where the GPU has `VK_KHR_video_encode_quantization_map` (RADV does, with 64x64
R32_SINT texels that only a linear, host-written image accepts), the encoder gets a QP delta map
that is round rather than rectangular: 0 inside the largest circle around the optical axis that
fits in the band, rising smoothly (in tangent space, so by angle from the axis) to +8 QP at the
edge of the view (`Foveation::falloff`). CBR still holds the bitrate, so the bits the edges give
up go to the center: in `--encode-selftest` at 60 Mbit/s, +8 QP on one half cost that half
1.2 dB PSNR and gave the other 0.5 dB at the same size. At a bitrate the content does not need,
RADV pads with filler instead and the center gains nothing. Without the extension the encoder
logs it and encodes at uniform quality. The parameters ride in every frame's
`FrameInfo`, so a picture is always unwarped with what it was warped with; changing the band size
changes the coded size, so the server rebuilds its encoder (the first frame is an IDR) and the
headset its decoder, but SteamVR's render size does not depend on it, so SteamVR keeps running.

## Audio

Both directions share the headset socket (`openlink_protocol::audio`): game audio stereo
server → headset, the microphone mono headset → server. Uncompressed s16 at 48 kHz in 5 ms
datagrams (1.5 and 0.8 Mbit/s, small next to the video), so neither side needs a codec. A
sequence number per stream lets the receiver fill a lost packet with silence and keep timing.

Each receiver has a `JitterBuffer`. It watches the lowest level it reaches over 2 s (the margin
it actually had against jitter) and steers that towards 10 ms by dropping or repeating single
frames, one per callback at most. That also follows the drift between the PC's and the
headset's audio clocks, which would otherwise slowly empty or overfill it. A big excess (a
burst, or a reader that paused) is dropped in one go.

| Side | Linux | Windows | Quest |
|---|---|---|---|
| Devices | PipeWire streams with `media.class` `Audio/Sink` and `Audio/Source` ("OpenLink"), created while a headset is connected | User mode cannot create endpoints: WASAPI loopback capture of the default output device (followed when it changes), and the microphone rendered into a virtual audio cable found by name ("CABLE Input" by default). A bundled virtual audio driver is the plan | n/a |
| I/O | process callbacks feed `audio::Bridge` | a 2 ms polling loop (with `timeBeginPeriod(1)` while streaming) feeds the same `Bridge`; WASAPI converts to 48 kHz s16 | AAudio low-latency streams; mic with the voice-communication preset (echo cancellation) |

The devices follow the headset: they appear when it connects and the server's settings enable
them, and vanish when it leaves, so the desktop falls back to the previous device. The desktop
remembers the choice by node name. The headset opens its streams when `Welcome` asks for them,
and reopens them if Android closes them (headphones plugged in).

## Wired connection

Over USB the headset is simply another IP route. Its USB NCM network interface (or
`adb forward`/`reverse` as a fallback, TCP only) appears as a network interface on the PC, so the
session code is identical. Only discovery differs. ADB forwarding cannot carry UDP, so NCM is the
preferred wired path.

## Open questions

- **Angular velocity frame.** Resolved: **local**. The OpenVR headers do not say whether
  `DriverPose_t::vecAngularVelocity` is in world or local space. Reading SteamVR's own
  prediction back (`GetRawTrackedDevicePoses`) while `mock-headset --spin-tilted` spins the
  controllers showed it computes `q * exp(w dt)`: within 0.02 deg of the local reading, 37 deg
  from the world one after 100 ms at 6 rad/s. Sending world-space velocities made swings
  predicted about the wrong axis (a controller swung from its butt end flopped instead of
  rotating about the hand).
- **Frame capture on Linux.** Confirmed in testing on SteamVR 2.19 under Wayland: the current
  `IVRDisplayComponent` ("display on desktop") path fails with `CHmdWindowSDL: VR requires direct
  mode` / `VRInitError_Compositor_CannotDRMLeaseDisplay`. vrserver and tracking work, but the
  compositor does not start, so Linux needs a direct mode driver before anything renders.
  Resolved: the HMD exposes `IVRDriverDirectModeComponent` on Linux and allocates swap textures
  with `IVRIPCResourceManagerClient::NewSharedVulkanImage`, and the compositor then runs at the
  headset refresh rate. That interface is only available once `vrcompositor` is up, after
  driver `Init`, so it is fetched lazily. Next step: import the textures into the encoder via
  `RefResource` + `ReceiveSharedFd`.
- **Display changes at runtime.** Resolved: they do not take. SteamVR reads the HMD's render
  target size and projection once, when the compositor starts; `SetRecommendedRenderTargetSize`
  and `SetDisplayProjectionRaw` later are accepted but the compositor keeps its swap textures
  and frustum (it stayed at the placeholder's 2876x3080 after a Quest 3 reported 1680x1760), so
  games render another FOV and aspect than the headset shows: severe distortion. The driver
  therefore saves the last headset's info (`last-headset.bin` in the platform config dir),
  describes that display from SteamVR's start, and asks SteamVR to restart
  (`IVRServerDriverHost::RequestRestart`) when a headset with a different size or FOV connects.
  The size the driver is given is the *encoded* per-eye size (the desktop app's "Encoded
  Resolution" percentage of the headset's panel resolution, looked up by model since OpenXR only
  reports the runtime's much smaller recommended size), enlarged for the lens. The lens
  concentrates the panel's pixels at the center of view, so a flat render target over the same
  FOV must be larger than the panel to match that density, which is why native headsets' 100% is
  above their panel size (Index: 1.4x). The matching size is the center pixels per radian times
  the FOV's tangent span; the lens profile is not queryable through OpenXR, so the center density
  comes from Meta's published PPD per model (`HeadsetInfo::render_size`; Quest 3: 25 PPD, 1.54x,
  about 3174x3429 at 100%; an unknown headset gets 1x). The converter downsamples SteamVR's
  frames to the encoded size, and SteamVR's render resolution setting scales from there. The
  server re-checks the settings every 500 ms while a headset is connected and, when the stream
  changes, sends the headset a new `Welcome` (it rebuilds its swapchains and decoder) and the
  driver a new `HeadsetConnected`, so saving a new resolution restarts SteamVR.
- **Quest client toolchain.** The plan is `openxr` + `ndk` crates with `cargo-apk`/`xbuild`. The
  `TrackingSource` trait in `openlink-client` is the seam: the OpenXR implementation slots in
  where `mock-headset` is today.

## Roadmap

1. **Skeleton (done).** Protocol, clock sync, prediction policy, SteamVR driver registering an
   HMD and Touch controllers, server relay, GUI, mock headset.
2. **Tracking on a real Quest.** OpenXR client app that samples at 500 Hz, plus input and haptics.
   Settle the angular velocity question. Compare swing feel against Steam Link.
3. **Video on Linux (done).** Direct mode capture as dmabufs, Vulkan Video H.265, UDP video
   transport, decode and reprojection on the headset.
4. **Windows (done, Vulkan Video).** The same pipeline over D3D11 shared textures, WASAPI
   audio. Next: testing on a real Quest and on NVIDIA/Intel, NVENC/AMF as alternatives, a
   virtual audio driver.
5. **Quality.** FEC and adaptive bitrate, foveated encoding (done), USB NCM discovery,
   Quest Pro face/eye tracking, hand tracking with skeletal input.
