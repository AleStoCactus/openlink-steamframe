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
| Steam Frame build | 📋 Planned | A client for Valve's Steam Frame |
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
crates/openlink-quest      Quest app (OpenXR, NativeActivity)
driver/openlink            SteamVR driver manifest/resources
scripts/install-driver.sh  build + register the driver (Linux)
scripts/build-quest.sh     build (and --install) the Quest APK
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

## License

GPL-3.0-only.
