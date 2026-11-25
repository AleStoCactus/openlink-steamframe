# OpenLink

PC VR streaming for standalone headsets that behaves like a native SteamVR headset: Link/Lighthouse-style
controller prediction, hardware H.264/HEVC/AV1 encoding on every GPU vendor, Wi-Fi or USB, Windows and Linux.

**Status: first end-to-end version (Linux PC + Quest 3).** SteamVR renders into the driver's textures,
the server encodes them with Vulkan Video (H.265) and streams them over UDP, and the Quest app
decodes them in hardware and shows them through OpenXR, reprojected to the current head pose. Head
and controller tracking, buttons and haptics flow back. There is no audio yet, no Windows video path,
and no forward error correction or adaptive bitrate. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

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
