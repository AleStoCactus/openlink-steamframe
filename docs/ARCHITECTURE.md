# OpenLink architecture

OpenLink streams SteamVR to standalone headsets (Quest 2/3/3S/Pro first) over Wi-Fi or USB.
The aim is a headset that behaves like a native SteamVR HMD:

- **Tracking** that feels like Link / Steam Link / Lighthouse: no overshooting "standalone-style"
  controller prediction.
- **Image quality and latency** at Virtual Desktop's level: zero-copy hardware encode with
  H.264, HEVC and AV1 on NVIDIA, AMD and Intel.
- **Windows and Linux** as equal targets.
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

## Video pipeline (next milestone)

| Stage | Windows | Linux |
|---|---|---|
| Frame capture | `IVRDriverDirectModeComponent` → shared D3D11 textures | Open question (see below) |
| Colour convert | compute shader to NV12/P010 on the GPU | same (Vulkan) |
| Encode | NVENC, AMF, QSV via their SDKs; Vulkan Video | NVENC, VA-API, Vulkan Video |
| Transport | UDP, slice-level packetisation, FEC, adaptive bitrate | same |
| Decode | `AMediaCodec` in low-latency mode on the Quest | n/a |
| Display | OpenXR swapchain plus reprojection with the render pose | n/a |

Encode must be zero-copy: the texture SteamVR hands the driver is encoded on the same GPU,
without a round trip through system memory.

AV1 encode is a hardware feature: RTX 40+, RX 7000+, Arc / Core Ultra. On older GPUs (e.g. an
RX 6800) the GUI offers H.264/HEVC only. No software can change that.

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
| Devices | PipeWire streams with `media.class` `Audio/Sink` and `Audio/Source` ("OpenLink"), created while a headset is connected | Planned: a bundled virtual audio driver (user mode cannot create endpoints), WASAPI on top; interim WASAPI loopback | n/a |
| I/O | process callbacks feed `audio::Bridge` | same `Bridge` | AAudio low-latency streams; mic with the voice-communication preset (echo cancellation) |

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
  reports the runtime's much smaller recommended size), so SteamVR's 100% renders exactly what is
  streamed and its render resolution setting supersamples from there. The server re-checks the
  settings every 500 ms while a headset is connected and, when the stream changes, sends the
  headset a new `Welcome` (it rebuilds its swapchains and decoder) and the driver a new
  `HeadsetConnected`, so saving a new resolution restarts SteamVR.
- **Quest client toolchain.** The plan is `openxr` + `ndk` crates with `cargo-apk`/`xbuild`. The
  `TrackingSource` trait in `openlink-client` is the seam: the OpenXR implementation slots in
  where `mock-headset` is today.

## Roadmap

1. **Skeleton (done).** Protocol, clock sync, prediction policy, SteamVR driver registering an
   HMD and Touch controllers, server relay, GUI, mock headset.
2. **Tracking on a real Quest.** OpenXR client app that samples at 500 Hz, plus input and haptics.
   Settle the angular velocity question. Compare swing feel against Steam Link.
3. **Video on Windows.** Direct mode capture, NVENC/AMF, UDP video transport, decode and
   reprojection on the headset.
4. **Video on Linux.** Capture spike, then VA-API/NVENC/Vulkan Video.
5. **Quality.** FEC and adaptive bitrate, foveated encoding, audio on Windows, USB NCM discovery,
   Quest Pro face/eye tracking, hand tracking with skeletal input.
