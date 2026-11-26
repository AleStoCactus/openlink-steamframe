# OpenLink

PC VR streaming for standalone headsets that behaves like a native SteamVR headset.

## Why

Streaming SteamVR to a standalone headset today means compromises. Steam Link is closed source
and does not work reliably on Linux, Virtual Desktop is Windows-only, and ALVR and similar
streamers predict controllers on the headset, so fast swings overshoot and snap back.
OpenLink aims for:

- **Tracking that feels like Link or Lighthouse:** the headset samples poses at "now" and SteamVR
  does the predicting, as it does for Index controllers. No floppy sabers.
- **A real SteamVR headset:** the driver sits in SteamVR's direct mode, so games, the dashboard
  and overlays behave as on a wired headset.
- **Zero-copy hardware encoding** on every GPU vendor, with **Linux and Windows** as equal targets.
- **Open source** (GPL-3.0): a new codebase, not derived from ALVR.

## Status

Early but playable: games run well on a Linux PC with a Quest 3 over Wi-Fi. SteamVR renders into
the driver's textures, the server encodes them with Vulkan Video (H.265) and streams them over UDP,
and the Quest app decodes them in hardware and shows them through OpenXR, reprojected to the
current head pose. Head and controller tracking, buttons and haptics flow back.
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

| Area | Status | Notes |
|---|---|---|
| Video | ✅ Working | H.265 via Vulkan Video, zero-copy from SteamVR, frame pacing locked to the headset's display. SteamVR's laser and overlays are drawn over games. No forward error correction or adaptive bitrate yet; H.264 and AV1 are planned |
| Audio | 📋 Planned | No game audio or microphone yet |
| Quest UI | 📋 Planned | The app connects on its own (LAN discovery) but shows nothing in the headset before the stream starts: no status, pairing or settings screen |
| Desktop UI | 🟡 Partial | `openlink` window shows driver, headset, clock-sync and tracking status, and holds video and connection settings. Only some settings take effect (H.265 is the only encoder) |
| Linux version | ✅ Working | The development platform. Needs a GPU with Vulkan Video H.265 encode (e.g. RADV on RX 6000+) |
| Windows version | 📋 Planned | Tracking and driver parts are written with Windows in mind, but there is no Windows video path yet |
| Steam Frame build | 🟡 Written, untested on hardware | Native arm64 Linux app (`openlink-frame`): OpenXR on the headset's SteamVR runtime, GL over EGL, hardware decode through V4L2 (Qualcomm Iris), zero-copy via dmabuf where the driver allows it. Checked against Monado and the real server, not yet on a Frame. See [Play on a Steam Frame](#play-on-a-steam-frame-untested) |
| Wi-Fi streaming | ✅ Working | See [Wi-Fi and USB cable](#wi-fi-and-usb-cable) |
| USB cable mode | 📋 Planned | The setting exists in the desktop app but does nothing yet |
| Full native SteamVR functionality | 🟡 Partial | Games, tracking, controllers, haptics, dashboard and overlays work. Known issue: SteamVR's own scene (the SteamVR void, or the dashboard open over a game) jitters with head motion, while games are smooth |

## Wi-Fi and USB cable

**Wi-Fi (working).** The default and the only mode in use today. The headset finds the PC by LAN
broadcast; to point it at a specific PC instead, run
`adb shell setprop debug.openlink.server 192.168.1.10`. For a clean stream:

- Connect the PC to the router by Ethernet and the headset over 5 GHz or 6 GHz Wi-Fi, ideally in
  the same room as the access point.
- Start around 150–200 Mbps and raise the bitrate (desktop app, applies on the next connection)
  while the stream stays free of stutter and artifacts. There is no adaptive bitrate or error
  correction yet, so a link that drops packets shows it as glitches.

**USB cable (planned).** The desktop app already has a "USB cable" setting, but nothing acts on
it yet. The plan: over USB the headset appears as another network interface on the PC (USB NCM),
so streaming works exactly as over Wi-Fi and only discovery changes. `adb forward`/`reverse`
cannot carry UDP, so it is not the wired path. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#wired-connection).

## Layout

```
crates/openlink-protocol   wire protocol, clock sync
crates/openlink-tracking   pose math + prediction policy
crates/openlink-driver     SteamVR driver (pure Rust)
crates/openlink-server     desktop app `openlink` (GUI + session)
crates/openlink-client     headset client core + `mock-headset`
crates/openlink-xr         OpenXR tracking, input and stream statistics shared by the headset apps
crates/openlink-quest      Quest app (OpenXR, NativeActivity)
crates/openlink-frame      Steam Frame app (native Linux arm64: OpenXR, EGL, V4L2)
driver/openlink            SteamVR driver manifest/resources
scripts/install-driver.sh  build + register the driver (Linux)
scripts/build-quest.sh     build (and --install) the Quest APK
scripts/build-frame.sh     build (and --install over SSH) the Steam Frame app
```

## Try it without a headset (Linux)

```sh
cargo test --workspace
scripts/install-driver.sh                       # builds and registers dist/openlink with SteamVR
cargo run --release -p openlink-server          # the desktop app
cargo run --release -p openlink-client --bin mock-headset
# now start SteamVR: an OpenLink HMD and two Touch controllers appear, swinging
```

Start SteamVR from Steam (or `steam steam://run/250820`), not by running `vrstartup.sh` directly. Without
Steam's runtime launcher service, `vrcompositor` cannot start.

`scripts/install-driver.sh --remove` unregisters the driver.

## Play on a Quest (Linux)

Needs a GPU with Vulkan Video H.265 encode (e.g. RADV on RX 6000+), and the Quest in developer mode
on the same LAN as the PC.

```sh
# once: Android toolchain (Arch) and the Rust target
sudo pacman -S --needed android-tools android-udev jdk17-openjdk
yay -S android-sdk-cmdline-tools-latest android-sdk-build-tools android-platform-34 android-ndk
rustup target add aarch64-linux-android && cargo install cargo-ndk

scripts/install-driver.sh
scripts/build-quest.sh --install                # builds, installs and starts the app over USB
cargo run --release -p openlink-server          # the desktop app
steam steam://run/250820                        # SteamVR
```

The app finds the server by LAN broadcast. To point it at a specific PC instead:
`adb shell setprop debug.openlink.server 192.168.1.10`. Logs: `adb logcat -s OpenLink`.

`openlink --dump-video out.h265` also writes the stream to a file; `openlink --encode-selftest
4128x2208 600 out.h265` exercises the encoder without SteamVR.

## Play on a Steam Frame (untested)

The Frame runs SteamOS on arm64, so its client is a native Linux program, not an APK. It renders
through SteamVR's OpenXR runtime on the headset (desktop GL over EGL, `XR_MNDX_egl_enable`) and
decodes with the Frame's hardware decoder (`/dev/video-dec0`, V4L2). The PC side is unchanged:
the same driver and `openlink` server as for a Quest. The Frame's controllers reach the PC as
Touch controllers (SteamVR on the headset maps them), so games' Touch bindings apply.

This build has not run on a Frame yet. It has been checked against Monado and the real server,
and its tests also pass on arm64, but the headset's decoder and SteamVR runtime are untested.
Please report the log (`~/.local/state/openlink/frame.log`) of a first try.

```sh
# on the PC: cross-build for arm64 (or run the same script on the headset itself)
sudo pacman -S --needed aarch64-linux-gnu-gcc    # Debian/Ubuntu: gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu
scripts/build-frame.sh --install 192.168.1.20    # the headset's address; needs sshd on it
```

On the headset (Developer Mode; `sudo systemctl enable --now sshd`, and set a password with
`passwd`), add `~/Applications/openlink/openlink-frame.sh` to Steam as a non-Steam game and start
it, or start it over SSH while SteamVR runs there. It finds the PC by LAN broadcast;
`--server 192.168.1.10` (or `OPENLINK_SERVER`) names it instead.

Useful switches: `OPENLINK_V4L2_DECODER=/dev/videoN` (another decoder node),
`OPENLINK_OPENXR_LOADER=/path/libopenxr_loader.so`, `OPENLINK_EGL_SURFACELESS=1`, `--swap-eyes`.
Without a decoder the app still runs, with tracking and input only, which is enough to test the
controllers.

## License

GPL-3.0-only.
