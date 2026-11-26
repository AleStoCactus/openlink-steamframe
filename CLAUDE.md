# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

OpenLink streams SteamVR to standalone headsets (Quest first) so they behave like a native SteamVR
headset. Rust workspace, GPL-3.0, a new codebase (not derived from ALVR). Linux is the development
platform; Windows is an equal target, so platform code sits behind `cfg` with Windows in mind.

## Commands

`cargo` lives in `~/.cargo/bin`, which may not be on PATH in non-login shells.

```sh
cargo build --workspace                       # host crates (openlink-quest compiles to nothing off Android)
cargo test --workspace
cargo test -p openlink-protocol audio::tests::follows_clock_drift_without_underruns   # one test
cargo clippy -p openlink-protocol -p openlink-client -p openlink-server

scripts/install-driver.sh                     # build + register the SteamVR driver (SteamVR must be closed)
cargo run --release -p openlink-server        # desktop app `openlink` (--headless: no window)
cargo run --release -p openlink-client --bin mock-headset   # fake headset; see flags in mock_headset.rs

# Quest (ANDROID_NDK_HOME=/opt/android-ndk)
cargo ndk -t arm64-v8a -P 32 build -p openlink-quest        # type-check / build the Android lib
scripts/build-quest.sh [--install]            # signed APK, optionally install + start over adb
adb logcat -s OpenLink
```

- Start SteamVR from Steam (`steam steam://run/250820`), not `vrstartup.sh`.
- Rerun `scripts/install-driver.sh` after any driver change **and after any `PROTOCOL_VERSION`
  bump**: the installed driver silently ignores messages of another version.
- `scripts/build-quest.sh` only builds the Android lib; `target/release/openlink` can be stale.
- Server debug: `openlink --dump-video out.h265`, `openlink --encode-selftest 4128x2208 600 out.h265`.
- Quest debug properties (`adb shell setprop …`): `debug.openlink.server <ip>` (skip LAN discovery),
  `debug.openlink.freeze 1`, `debug.openlink.swap_eyes 1`.
- Server build needs PipeWire headers and clang (pipewire crate, bindgen); the video path needs
  a GPU with Vulkan Video H.265 encode.

## Architecture

Three processes. Read `docs/ARCHITECTURE.md` for the reasoning behind tracking and prediction.

```
Quest app (openlink-quest + openlink-client)  ──UDP 9757──  server `openlink` (openlink-server)  ──loopback UDP 9758/9759──  SteamVR driver (openlink-driver, inside vrserver)
```

- **Driver is deliberately thin.** It runs inside `vrserver`, so a panic there kills SteamVR:
  every entry point goes through `guard()` (catch_unwind). It has no network access to the
  headset, no encoder, no UI. OpenVR C++ vtables are written by hand in `ovr.rs` (layout-tested);
  there is no C++ or bindgen.
- **Server owns everything with failure modes**: headset session, clock sync, relay to the driver,
  encoder, audio devices, egui GUI, settings (`config.rs`, JSON in the platform config dir; most
  changes apply on the next headset connection). `session.rs` runs the headset and driver
  threads; `HeadsetLink` is the handle the video and audio services use to reach the headset
  over the session's own socket.
- **Protocol crate** (`openlink-protocol`) is shared by all three. Control messages are
  `[MAGIC][PROTOCOL_VERSION][postcard]`; video (`OLVD`) and audio (`OLAU`) have their own
  datagram framing on the same headset socket, so receivers check `video::is_video` /
  `audio::parse` before decoding control messages. Bump `PROTOCOL_VERSION` on any wire change.
- **Clock domain**: the headset samples tracking at "now" (500 Hz, never predicting); the server
  converts timestamps into the PC monotonic clock (`protocol::clock`, `time::monotonic_ns`) before
  forwarding, and the driver reports them with a negative `poseTimeOffset` so SteamVR does the
  predicting. Angular velocity is in **local** space. Controllers are real `oculus_touch` devices
  posed at the Touch raw pose (grip offset conversion in `openlink-tracking`).
- **Video (Linux)**: the HMD is an `IVRDriverDirectModeComponent`. The driver allocates the
  compositor's textures as GBM dmabufs and passes the fds to the server over an abstract-namespace
  SEQPACKET socket (`video_ipc`); the server imports them into Vulkan, converts to NV12 and encodes
  H.265 with Vulkan Video, zero-copy. Each frame carries the pose it was rendered with
  (`render_pose.rs` corrects SteamVR's one-frame-stale compositor pose) and the Quest reprojects
  from it. The headset reports display timing back so the server paces SteamVR's vsync and
  photon latency to it.
- **Audio**: 48 kHz s16 PCM in 5 ms datagrams both ways, with an adaptive `JitterBuffer`
  (`protocol/src/audio.rs`) on each receiver. Linux server creates PipeWire `Audio/Sink` and
  `Audio/Source` streams named "OpenLink" only while a headset is connected
  (`server/src/audio/pipewire.rs`); platform-neutral logic is in `audio::Bridge`; Windows is a stub.
  Quest uses AAudio (`quest/src/audio.rs`) and requests `RECORD_AUDIO` via JNI.
- **Headset client** (`openlink-client`) is platform-independent: the `TrackingSource` trait is the
  seam between the OpenXR implementation (`openlink-quest/src/tracking.rs`) and `mock-headset`,
  which exercises the whole PC side without a headset.
- **Quest app** (`openlink-quest`, `#![cfg(target_os = "android")]`): NativeActivity + OpenXR on the
  main thread, with tracking/network, decoder (AMediaCodec) and audio threads beside it.

## Conventions

- Comments and docs explain *why* (measurements, SteamVR quirks) in full sentences; keep that
  style, and update `README.md`'s status table and `docs/ARCHITECTURE.md` when behaviour changes.
- Linux-only modules are gated with `#[cfg(target_os = "linux")]`; keep Windows builds possible.
