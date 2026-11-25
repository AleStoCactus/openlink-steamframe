#!/usr/bin/env bash
# Build the SteamVR driver, assemble the driver folder in dist/openlink and register it with
# SteamVR. Run again after every driver change (SteamVR must be closed).
#
#   scripts/install-driver.sh            # build release + register
#   scripts/install-driver.sh --remove   # unregister
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dist="$root/dist/openlink"
steamvr="${STEAMVR_PATH:-$HOME/.steam/steam/steamapps/common/SteamVR}"
vrpathreg="$steamvr/bin/vrpathreg.sh"

if [[ ! -x "$vrpathreg" ]]; then
    echo "SteamVR not found at $steamvr (set STEAMVR_PATH)" >&2
    exit 1
fi

if [[ "${1:-}" == "--remove" ]]; then
    "$vrpathreg" removedriver "$dist"
    echo "unregistered $dist"
    exit 0
fi

if pgrep -x vrserver >/dev/null; then
    echo "SteamVR is running; close it first (vrserver keeps the old driver loaded)." >&2
    exit 1
fi

cargo build --release -p openlink-driver --manifest-path "$root/Cargo.toml"

rm -rf "$dist"
mkdir -p "$dist/bin/linux64"
cp -r "$root/driver/openlink/." "$dist/"
cp "$root/target/release/libdriver_openlink.so" "$dist/bin/linux64/driver_openlink.so"

"$vrpathreg" adddriver "$dist"
echo "registered $dist"
echo "Start 'openlink' (or 'cargo run -p openlink-server'), then SteamVR."
