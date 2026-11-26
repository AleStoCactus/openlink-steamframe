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

Early but playable: games run well on a Linux PC with a Quest 3 over Wi-Fi, and on a Steam Frame
over Wi-Fi or its USB wireless adapter; the Windows version runs the same pipeline (tested with a
Quest 3 on an AMD GPU). SteamVR renders into the driver's textures, the server encodes them with
Vulkan Video (H.265) and streams them over UDP, and the headset app decodes them in hardware and
shows them through OpenXR, reprojected to the current head pose. Head and controller tracking,
buttons and haptics flow back. Game audio plays in the headset and its microphone appears on the PC.
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

| Area | Status | Notes |
|---|---|---|
| Video | ✅ Working | H.265 via Vulkan Video, zero-copy from SteamVR, frame pacing locked to the headset's display. SteamVR's laser and overlays are drawn over games, but see [SteamVR's layer over games](#known-issue-steamvrs-layer-over-games). Optional foveated encoding: a slider sets how much of each eye around the lens center is encoded at full resolution, with resolution falling off smoothly to a quarter at the edges, so fewer pixels are encoded (about half at 50%), and, on GPUs with quantization maps (e.g. RADV), fewer bits going to a ring around the lens center that widens toward the edges; it changes live without restarting SteamVR. No forward error correction or adaptive bitrate yet; H.264 and AV1 are planned |
| Audio | 🟡 Partial | Linux: while a headset is connected, PipeWire gets an "OpenLink" output device (plays in the headset) and an "OpenLink" microphone, to pick in the sound settings. Windows (which cannot create devices without a kernel driver): what plays on the default output device is also streamed to the headset, and the headset's microphone plays into a virtual audio cable (VB-CABLE's "CABLE Input" by default, settable in the desktop app) whose other end is the microphone to pick. Uncompressed 48 kHz PCM with an adaptive jitter buffer; echo cancellation on the headset mic |
| Quest UI | 📋 Planned | The app connects on its own (LAN discovery) but shows nothing in the headset before the stream starts: no status, pairing or settings screen |
| Desktop UI | 🟡 Partial | `openlink` window shows driver, headset, clock-sync, tracking and Steam Frame adapter status, and holds video, audio and connection settings, with level meters for the headset's audio. The encoded resolution is set in percent of the headset's panel (Quest 3 at 100%: 2064×2208 per eye) and applies live; SteamVR's 100% is that size enlarged for the lens, like a native headset's (Quest 3 at 100%: about 3174×3429, matching the 25 PPD at the center of view; on a Steam Frame, whose lens profile is not known yet, it is the encoded size), and its render resolution setting scales from there. Refresh rates are 72 to 120 Hz, plus 144, 165, 207 and 240 Hz with a Quest 3 (Horizon OS v2.7 or later; 240 Hz only in developer mode with display scaling on). The Quest runtime does not list these rates but accepts them when asked, so the app tries each at startup; the stream uses the closest rate the headset accepts. Only some settings take effect (H.265 is the only encoder; USB cable does nothing yet) |
| Linux version | ✅ Working | The development platform. Needs a GPU with Vulkan Video H.265 encode (e.g. RADV on RX 6000+; also tested on an RX 9070 XT). With a second GPU (an integrated one, say) the driver uses the one SteamVR's compositor renders on; `OPENLINK_RENDER_NODE=/dev/dri/renderDN` overrides it |
| Windows version | 🟡 Partial | The same pipeline as on Linux: the driver shares SteamVR's frames as D3D11 textures, the server encodes them with Vulkan Video (H.265). Verified with a Quest 3 over Wi-Fi on an AMD RX 6800 (90 fps at full resolution with nothing skipped: convert 1–1.6 ms, encode 10.3 ms, overlapped; 3.3 ms encode with foveation); not yet on NVIDIA or Intel. AMD's Windows driver ignores Vulkan's bitrate setting, so on Windows the server sets each frame's QP itself. SteamVR's own render resolution setting applies on top: check it if the picture looks softer than on Linux. Needs a GPU whose Windows driver has Vulkan Video H.265 encode, and the Vulkan SDK to build |
| Steam Frame build | 🟡 Playable | Native arm64 Linux app (`openlink-frame`): OpenXR on the headset's SteamVR runtime, GL over EGL, hardware decode through V4L2 (Qualcomm Iris, 3–6 ms a picture). Beat Saber plays. Controllers still show up as Quest Touch controllers. See [Play on a Steam Frame](#play-on-a-steam-frame) |
| Wi-Fi streaming | ✅ Working | See [Connection](#connection) |
| Steam Frame adapter | 🟡 Working, rough edges | The Frame's USB wireless adapter as a direct 6 GHz link. See [Connection](#connection) |
| USB cable mode | 📋 Planned | The setting exists in the desktop app but does nothing yet |
| Quest 3 Extended Refresh Rates | 🟡 Partial | Server side works, Quest app doesn't request the refresh rate correctly |
| Full native SteamVR functionality | 🟡 Partial | Games, tracking, controllers, haptics, dashboard and overlays work. Known issues: SteamVR's own scene (the SteamVR void, or the dashboard open over a game) jitters with head motion, while games are smooth; [SteamVR's layer over games](#known-issue-steamvrs-layer-over-games) |

### Planned

Beyond the rows above, these are on the way:

**Video and display**

- **Automatic refresh rate and resolution detection:** the refresh rates and resolution the
  headset supports are read from it and offered as options, instead of being entered by hand.

**SteamVR integration**

- **The real headset model** (e.g. Quest 3) reported in SteamVR, not a generic OpenLink HMD.
- **Battery status** for the headset and both controllers in the SteamVR dashboard.
- **SteamVR throttling options** that work: the behavior chosen in SteamVR's per-app settings is
  applied to the stream.

**Desktop app**

- **Driver installation from the GUI**, so `scripts/install-driver.sh` (`.ps1` on Windows) is no
  longer needed.
- **A clearer, more intuitive layout** overall.

## Connection

**Wi-Fi (working).** The default. The headset finds the PC by broadcasting on every network it is
on; to point it at a specific PC instead, run `adb shell setprop debug.openlink.server
192.168.1.10` (Quest) or pass `--server 192.168.1.10` (Steam Frame). For a clean stream:

- Connect the PC to the router by Ethernet and the headset over 5 GHz or 6 GHz Wi-Fi, ideally in
  the same room as the access point.
- Start around 150–200 Mbps on a Quest, 100 Mbps on a Steam Frame, and raise the bitrate (desktop
  app, applies on the next connection) while the stream stays free of stutter and artifacts. There
  is no adaptive bitrate or error correction yet, so a link that drops packets shows it as
  glitches, and one that drops part of every keyframe shows nothing at all.
- On a Steam Frame, 200 Mbps over home Wi-Fi showed only black, most likely because Linux caps the
  app's UDP receive buffer at `net.core.rmem_max` (about 200 KB by default), less than one frame
  at that bitrate. The app
  logs a warning when its buffer is capped; `sudo sysctl -w net.core.rmem_max=8388608` on the
  headset lifts the cap until the next reboot.

**Steam Frame adapter (working, rough edges).** The Steam Frame comes with a USB wireless adapter
(Valve 28de:2432, a Realtek 6 GHz card) that joins the headset's own access point: a direct link
beside the home network (measured on one setup: 1.7 Gbit/s link rate at 160 MHz, 2.7 ms ping
against 3.5 ms, with less jitter, over home Wi-Fi). Pick "Steam Frame adapter" under Connection in
the desktop app. OpenLink then brings up the NetworkManager profile Steam made when it paired the
headset (a hidden SSID `frame_…`; pair the headset with Steam once if it is missing), shows the
link in the status, and takes the headset only on that network. The headset's access point must be
on (it is while the Frame's PC link is active). Without NetworkManager, join the access point by
hand and start the Frame app with `--server` and the PC's address on that network.

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
scripts/install-driver.ps1 build + register the driver (Windows)
scripts/build-quest.sh     build (and --install) the Quest APK (Linux)
scripts/build-quest.ps1    build (and -Install) the Quest APK (Windows)
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

`openlink --dump-video out.h265` also writes the stream to a file, and beside it `out.poses`: per
frame its render pose and the textures it was made from. `openlink --encode-selftest 4128x2208 600
out.h265` exercises the encoder without SteamVR.

## Play on a Steam Frame

The Frame runs SteamOS on arm64, so its client is a native Linux program, not an APK. It renders
through SteamVR's OpenXR runtime on the headset (desktop GL over EGL, `XR_MNDX_egl_enable`) and
decodes with the Frame's hardware decoder (`/dev/video-dec0`, V4L2). The PC side is unchanged:
the same driver and `openlink` server as for a Quest. The Frame's controllers reach the PC as
Touch controllers (SteamVR on the headset maps them), so games' Touch bindings apply.

It has been played on a Frame (SteamVR/OpenXR 2.19.5 on the headset), over home Wi-Fi and over the
Frame's adapter. The decoder is Qualcomm Iris at `/dev/video-dec0` (H.264, HEVC, VP9, AV1). The
log is `~/.local/state/openlink/frame.log`; please attach it to bug reports.

```sh
# on the PC: cross-build for arm64 (or run the same script on the headset itself)
sudo pacman -S --needed aarch64-linux-gnu-gcc    # Debian/Ubuntu: gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu      # Arch's Rust without rustup: sudo pacman -S rust-aarch64-gnu
scripts/build-frame.sh --install 192.168.1.20    # the headset's address; needs sshd on it
```

A cross-built binary needs at least the glibc of the cross toolchain, which the script prints
(`openlink-frame needs glibc 2.43 or newer`); compare it with `ldd --version` on the headset. If
the headset's is older the app does not start (`GLIBC_2.xx not found`); build on the headset then.

On the headset (Developer Mode; `sudo systemctl enable --now sshd`, and set a password with
`passwd`), add `~/Applications/openlink/openlink-frame.sh` to Steam as a non-Steam game and start
it, or start it over SSH while SteamVR runs there. It finds the PC by LAN broadcast;
`--server 192.168.1.10` (or `OPENLINK_SERVER`) names it instead.

Useful switches: `OPENLINK_V4L2_DECODER=/dev/videoN` (another decoder node),
`OPENLINK_OPENXR_LOADER=/path/libopenxr_loader.so`, `OPENLINK_EGL_SURFACELESS=1`, `--swap-eyes`.
Without a decoder the app still runs, with tracking and input only, which is enough to test the
controllers; `OPENLINK_V4L2_DECODER=/nonexistent` forces that.

Known on the Frame:

- The controllers show up in SteamVR as Quest Touch controllers, with Quest models. SteamVR on the
  PC already ships a `frame_controller` driver (models, input profile, bindings); using it needs
  the Frame controller profile on the headset and a matching device in the driver.
- SteamVR's runtime on the Frame has listed only 144 Hz while running the display at 72 Hz, and
  refused the switch. The app now also reports the rate the display runs at, so the PC paces and
  encodes at the real rate.
- Present-to-photon is about 36–40 ms, most of it between decoding and the display.

## Known issue: SteamVR's layer over games

While a game runs, SteamVR submits its own layer (dashboard, laser, controller models) next to the
game's, and the server draws it over the game. On an RX 9070 XT, SteamVR drew that layer into only
one of the three textures it rotates through, so the other two (stale, opaque) covered the game
in two frames of three: half the frame rate with black in between. Until that is understood,
turning off "SteamVR layer over games" in the desktop app's Display tab (saved as
`"steamvr_layer_over_games": false` in `~/.config/openlink/settings.json`) streams games without
SteamVR's layer: games are clean, but the dashboard opened over a game does not show in the
headset.

## Windows

Needs a GPU whose Windows driver has Vulkan Video H.265 encode (tested: AMD RX 6800, driver
32.0.21045), Rust with the MSVC toolchain, and the Vulkan SDK (for `glslc`;
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

:D
