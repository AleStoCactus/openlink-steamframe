# OpenLink

PC VR streaming for standalone headsets that behaves like a native SteamVR headset: Link/Lighthouse-style
controller prediction, hardware H.264/HEVC/AV1 encoding on every GPU vendor, Wi-Fi or USB, Windows and Linux.

**Status: early skeleton.** Tracking flows from a mock headset through the server to the driver's
loopback endpoint. The driver has not yet been loaded in a live SteamVR session. There is no video yet, and
no Quest app yet. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and roadmap.

## Layout

```
crates/openlink-protocol   wire protocol, clock sync
crates/openlink-tracking   pose math + prediction policy
crates/openlink-driver     SteamVR driver (pure Rust)
crates/openlink-server     desktop app `openlink` (GUI + session)
crates/openlink-client     headset client core + `mock-headset`
driver/openlink            SteamVR driver manifest/resources
scripts/install-driver.sh  build + register the driver (Linux)
```

## Try it without a headset (Linux)

```sh
cargo test --workspace
scripts/install-driver.sh                       # builds and registers dist/openlink with SteamVR
cargo run --release -p openlink-server          # the desktop app
cargo run --release -p openlink-client --bin mock-headset
# now start SteamVR: an OpenLink HMD and two Touch controllers appear, swinging
```

`scripts/install-driver.sh --remove` unregisters the driver.

## License

GPL-3.0-only.
