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

Early but playable: games run well on a Linux PC with a Quest 3 over Wi-Fi, and the Windows
version runs the same pipeline (tested with a Quest 3 on an AMD GPU). SteamVR renders into
the driver's textures, the server encodes them with Vulkan Video (H.265) and streams them over UDP,
and the Quest app decodes them in hardware and shows them through OpenXR, reprojected to the
current head pose. Head and controller tracking, buttons and haptics flow back. Game audio plays
in the headset and its microphone appears on the PC.
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Road to 1.0.0

What 1.0.0 needs, and where each operating system stands. The Quest app is the same for both, so
headset-side rows only differ where the PC side does.

✅ Done · 🟡 Partial · 📋 Planned · ❔ Untested

| Feature | Windows | Linux |
|---|---|---|
| Audio and video streaming | 🟡 Video works; audio needs a virtual cable for the mic and mirrors the default output | ✅ Done |
| Wi-Fi streaming | ✅ Done | ✅ Done |
| Wired streaming (USB) | 📋 Planned | 📋 Planned |
| SteamVR dashboard | ✅ Works correctly | 🟡 Functional, but jitters and glitches a lot |
| SteamVR throttling behavior support | 📋 Planned | 📋 Planned |
| High performance | 🟡 Full-resolution 90 fps on an RX 6800; no error correction or adaptive bitrate | 🟡 Full-resolution 90 fps on RX 6000+; no error correction or adaptive bitrate |
| Guided GUI setup | 📋 Planned | 📋 Planned |
| Better GUI | 🟡 Status and settings work; layout needs rework | 🟡 Status and settings work; layout needs rework |
| Quest GUI | 📋 Planned | 📋 Planned |
| AMD GPU support | ✅ Done (RX 6800) | ✅ Done (RADV, RX 6000+) |
| NVIDIA GPU support | ❔ Untested | ❔ Untested |
| Battery status in SteamVR | 🟡 Headset works; Quest controllers' charge is not available to apps | 🟡 Headset works; Quest controllers' charge is not available to apps |
| Correct headset model in SteamVR and games | ✅ Done | ✅ Done |
| Refresh rate picked live in SteamVR or the desktop app | ❔ Untested | ✅ Done |
| Statistics screen | 📋 Planned | 📋 Planned |

What the rows mean:

- **Audio and video streaming:** H.265 through Vulkan Video, zero-copy from SteamVR, with frame
  pacing locked to the headset's display; SteamVR's laser and overlays are drawn over games.
  Audio is uncompressed 48 kHz PCM with an adaptive jitter buffer and echo cancellation on the
  headset mic. Linux gets "OpenLink" PipeWire output and microphone devices while a headset is
  connected. Windows cannot create audio devices without a kernel driver, so it streams whatever
  plays on the default output and plays the headset's microphone into a virtual audio cable
  (see [Windows](#windows)).
- **Wired streaming:** the desktop app has a "USB cable" setting that does nothing yet. See
  [Wi-Fi and USB cable](#wi-fi-and-usb-cable).
- **SteamVR throttling behavior:** the behavior chosen in SteamVR's per-app settings is applied
  to the stream.
- **High performance:** measured on an RX 6800 under Windows: convert 1–1.6 ms and encode
  10.3 ms per frame, overlapped (3.3 ms encode with foveated encoding). Missing: forward error
  correction and adaptive bitrate, so a link that drops packets shows glitches.
- **Guided GUI setup:** install the driver from the desktop app instead of
  `scripts/install-driver.sh` (`.ps1` on Windows), and walk through pairing the headset.
- **Better GUI:** the `openlink` window shows driver, headset, clock-sync and tracking status,
  holds the video, audio and connection settings and has level meters for the headset's audio,
  but needs a clearer layout.
- **Quest GUI:** the app connects on its own (LAN discovery) but shows nothing in the headset
  before the stream starts: no status, pairing or settings screen.
- **GPU support:** any GPU whose driver has Vulkan Video H.265 encode should work. AMD is
  verified on both systems; NVIDIA and Intel are not tested yet.
- **Headset model:** the headset and controllers are named as Meta's own driver (Link) names
  them, with the Quest status icons, so SteamVR and games see e.g. a Meta Quest 3 with Touch
  Plus controllers. The controllers are the ones actually in use (Touch, Touch Plus or Touch
  Pro, from the OpenXR interaction profile), not guessed from the headset.
- **Battery status:** the headset's and both controllers' charge in the SteamVR dashboard (and
  the desktop app), sent every 5 s. The headset's comes from Android's `BatteryManager`; the
  controllers' only from runtimes with `XR_EXT_interaction_profile_battery_state_display`, and
  without it they show no battery rather than a wrong one. Meta's runtime does not offer that
  extension, and the system service that knows the controllers' charge refuses other apps, so
  on a Quest only the headset's battery shows until Meta adds it.
- **Refresh rate:** 72 to 120 Hz, plus 144, 165, 207 and 240 Hz on a Quest 3 (Horizon OS v2.7
  or later; 240 Hz only in developer mode with display scaling on). The Quest runtime does not
  list the higher rates but accepts them when asked; the stream uses the closest rate the
  headset accepts. The rates are offered in SteamVR's video settings like a native headset's,
  and picking one there or in the desktop app switches the headset, the encoder and SteamVR
  at once; the two settings follow each other. A rate forced on the headset
  (`debug.oculus.refreshRate`, set by tools such as Singularity) overrides every app's request,
  so the headset stays at it until it is set back to 0 and OpenLink restarted.
- **Statistics screen:** latency, bitrate, frame loss and encode/decode times in the desktop app.

Known issue (Linux Only): SteamVR's own scene (the SteamVR void, or the dashboard open over a game) jitters
with head motion, while games are smooth.

Already working on both systems: tracking (head and controllers, predicted by SteamVR),
buttons, haptics, the SteamVR dashboard and overlays, and foveated encoding. SteamVR's 100%
is the panel enlarged for the lens, like a native headset's (Quest 3: 3172×3428 per eye), and
the stream's "Center resolution" is relative to it: at 100% the center of view is streamed
1:1, as sharp as the lens shows it, while foveation squeezes the edges (default 25%: a
1746×1886 picture per eye, smaller than the panel). SteamVR's render resolution setting
supersamples on top. On Windows, check SteamVR's render resolution if the picture looks softer
than on Linux.

After 1.0.0: H.264 and AV1 encoding, and automatic detection of the refresh rates and
resolutions the headset supports. A Steam Frame or Pico client is not planned.

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
scripts/install-driver.ps1 build + register the driver (Windows)
scripts/build-quest.sh     build (and --install) the Quest APK (Linux)
scripts/build-quest.ps1    build (and -Install) the Quest APK (Windows)
```

## Try it without a headset (Linux)

```sh
cargo test --workspace
scripts/install-driver.sh                       # builds and registers dist/openlink with SteamVR
cargo run --release -p openlink-server          # the desktop app
cargo run --release -p openlink-client --bin mock-headset
# now start SteamVR: an OpenLink HMD and two Touch controllers appear, swinging
```

While the mock headset runs, the PC has "OpenLink" audio devices: what plays on the output is
logged by the mock (`--audio-out PATH` saves it), and `--mic-tone` feeds a 440 Hz tone into the
microphone.

Start SteamVR from Steam (or `steam steam://run/250820`), not by running `vrstartup.sh` directly. Without
Steam's runtime launcher service, `vrcompositor` cannot start.

`scripts/install-driver.sh --remove` unregisters the driver.

## Play on a Quest (Linux)

Needs a GPU with Vulkan Video H.265 encode (e.g. RADV on RX 6000+), PipeWire (building needs its
headers and clang), and the Quest in developer mode on the same LAN as the PC.

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

The app asks for the microphone permission on first start (or
`adb shell pm grant org.openlink.quest android.permission.RECORD_AUDIO`). To hear the game in the
headset and use its microphone, pick the "OpenLink" output and input devices in the PC's sound
settings; the choice is remembered for the next connection. Both can be turned off in the desktop
app.

The app finds the server by LAN broadcast. To point it at a specific PC instead:
`adb shell setprop debug.openlink.server 192.168.1.10`. Logs: `adb logcat -s OpenLink`.

`openlink --dump-video out.h265` also writes the stream to a file; `openlink --encode-selftest
4128x2208 600 out.h265` exercises the encoder without SteamVR.

## Windows

Needs a GPU whose Windows driver has Vulkan Video H.265 encode (tested: AMD RX 6800, driver
26.11.2), Rust with the MSVC toolchain, and the Vulkan SDK (for `glslc`;
`winget install KhronosGroup.VulkanSDK`). In PowerShell:

```powershell
cargo test --workspace
scripts\install-driver.ps1                      # builds and registers dist\openlink with SteamVR
cargo run --release -p openlink-server          # the desktop app
cargo run --release -p openlink-client --bin mock-headset   # or the Quest app
Start-Process steam://run/250820                # SteamVR
```

Allow `openlink` through Windows Firewall when asked (the headset reaches it on UDP 9757).
`scripts\install-driver.ps1 -Remove` unregisters the driver.

To build and install the Quest app from Windows, with the headset in developer mode on USB:

```powershell
# once: JDK, Android SDK + NDK (sdkmanager is in the SDK's cmdline-tools), Rust target, cargo-ndk
winget install Microsoft.OpenJDK.17
# sdkmanager.bat splits arguments at ';', so the packages go in a file
Set-Content packages.txt "build-tools;35.0.1`nplatforms;android-34`nndk;30.0.16248370"
sdkmanager --package_file=packages.txt
rustup target add aarch64-linux-android; cargo install cargo-ndk

scripts\build-quest.ps1 -Install                # builds, installs and starts the app over USB
```

It looks for the SDK in `ANDROID_HOME` or `%LOCALAPPDATA%\Android\Sdk`. An app built on another
machine is signed with that machine's debug key and must be uninstalled first
(`adb uninstall org.openlink.quest`), unless both use the same `~/.android/debug.keystore`.

Audio differs from Linux because Windows cannot create audio devices without a kernel driver:
game audio is whatever plays on the **default output device** (it keeps playing on the PC too),
and the headset's microphone plays into a **virtual audio cable** such as
[VB-CABLE](https://vb-audio.com/Cable/): install it, then pick "CABLE Output" as the microphone in
games. The desktop app's "Microphone plays into" setting names a different cable if you use one.

## License

GPL-3.0-only.
