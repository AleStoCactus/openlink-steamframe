#!/usr/bin/env bash
# Builds the Steam Frame app (Linux arm64) into a tarball, and with --install HOST copies it to
# the headset over SSH (Developer Mode, sshd enabled).
#
#   scripts/build-frame.sh [--debug] [--install HOST]
#
# On the headset itself (an arm64 machine) this is a plain cargo build. Anywhere else it
# cross-compiles: it needs the Rust target aarch64-unknown-linux-gnu (rustup adds it; with a
# distribution's Rust, install its package, e.g. Arch: rust-aarch64-gnu) and an aarch64 GCC
# (aarch64-linux-gnu-gcc; Arch: aarch64-linux-gnu-gcc, Debian/Ubuntu: gcc-aarch64-linux-gnu).
# The binary links only glibc; EGL and the OpenXR loader are loaded at run time from the
# headset's SteamOS and SteamVR.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE=release
HOST=""
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) PROFILE=debug ;;
        --install) HOST="${2:?--install needs the address of the headset}"; shift ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

TARGET=aarch64-unknown-linux-gnu
cd "$ROOT"
CARGO_FLAGS=(-p openlink-frame)
[ "$PROFILE" = release ] && CARGO_FLAGS+=(--release)
if [ "$(uname -m)" = aarch64 ]; then
    cargo build "${CARGO_FLAGS[@]}"
    BIN="target/$PROFILE/openlink-frame"
else
    LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-aarch64-linux-gnu-gcc}"
    command -v "$LINKER" >/dev/null || { echo "need $LINKER to cross-compile for the Steam Frame" >&2; exit 1; }
    if command -v rustup >/dev/null; then
        rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"
    elif [ ! -d "$(rustc --print sysroot)/lib/rustlib/$TARGET" ]; then
        # A distribution's Rust without rustup ships targets as packages.
        echo "Rust has no $TARGET target (Arch: pacman -S rust-aarch64-gnu)" >&2; exit 1
    fi
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$LINKER" cargo build --target "$TARGET" "${CARGO_FLAGS[@]}"
    BIN="target/$TARGET/$PROFILE/openlink-frame"
    # The cross toolchain's glibc can be newer than the headset's; the binary then fails with
    # "GLIBC_2.xx not found". Show what it needs (compare with `ldd --version` on the headset).
    if command -v readelf >/dev/null; then
        NEEDS="$(readelf -V "$BIN" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
        echo "openlink-frame needs glibc ${NEEDS#GLIBC_} or newer"
    fi
fi

OUT="$ROOT/target/frame"
STAGE="$OUT/openlink"
rm -rf "$STAGE"; mkdir -p "$STAGE"
cp "$BIN" "$STAGE/openlink-frame"
cat > "$STAGE/openlink-frame.sh" <<'SH'
#!/usr/bin/env bash
# Starts OpenLink on the Steam Frame; add this file to Steam as a non-Steam game.
# Logs: ~/.local/state/openlink/frame.log. Set OPENLINK_SERVER=192.168.1.10 below (or pass
# --server) to skip LAN discovery.
DIR="$(cd "$(dirname "$0")" && pwd)"
LOG="${XDG_STATE_HOME:-$HOME/.local/state}/openlink"
mkdir -p "$LOG"
export RUST_LOG="${RUST_LOG:-info}"
exec "$DIR/openlink-frame" "$@" > >(tee "$LOG/frame.log") 2>&1
SH
chmod +x "$STAGE/openlink-frame.sh"
tar -C "$OUT" -czf "$OUT/openlink-frame-arm64.tar.gz" openlink
echo "Built $OUT/openlink-frame-arm64.tar.gz"

if [ -n "$HOST" ]; then
    case "$HOST" in *@*) ;; *) HOST="steamos@$HOST" ;; esac
    ssh "$HOST" 'mkdir -p ~/Applications'
    scp "$OUT/openlink-frame-arm64.tar.gz" "$HOST:/tmp/openlink-frame-arm64.tar.gz"
    ssh "$HOST" 'tar -C ~/Applications -xzf /tmp/openlink-frame-arm64.tar.gz && rm /tmp/openlink-frame-arm64.tar.gz'
    echo "Installed to $HOST:~/Applications/openlink. Add ~/Applications/openlink/openlink-frame.sh to Steam"
    echo "as a non-Steam game, or with SteamVR running on the headset: ssh $HOST ~/Applications/openlink/openlink-frame.sh"
fi
