#!/usr/bin/env bash
# Build the Hearth Unity native plugin for every platform this host can build.
#
#   scripts/build-unity-native.sh                 # every buildable platform
#   scripts/build-unity-native.sh --only windows  # one of: windows linux macos android ios webgl
#   scripts/build-unity-native.sh --crate examples/arena-unity-plugin \
#       --out examples/arena-unity/Assets/Plugins/Hearth          # a game's own plugin
#
# Without --crate this builds the stock plugin (crates/hearth-unity-plugin) into
# the package's Plugins/ (WebGL core: Runtime/StreamingAssets~). With --out,
# binaries go to <out>/<Platform>/ and the WebGL core to <out>/WebGL~, which the
# package's WebGL build hook prefers over its own.
#
# Targets that cannot be built on this host are skipped with the reason.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG="$ROOT/unity/com.hearth.client"
CRATE_DIR="$ROOT/crates/hearth-unity-plugin"
OUT=""
ONLY=""

while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --crate) CRATE_DIR="$(cd "$2" && pwd)"; shift 2 ;;
        --out) mkdir -p "$2"; OUT="$(cd "$2" && pwd)"; shift 2 ;;
        *) echo "usage: $0 [--only windows|linux|macos|android|ios|webgl] [--crate <dir>] [--out <dir>]" >&2; exit 2 ;;
    esac
done

TARGET_DIR="$(cargo metadata --manifest-path "$CRATE_DIR/Cargo.toml" --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's#\\\\#/#g')"
PLUGINS="${OUT:-$PKG/Plugins}"
if [ -n "$OUT" ]; then WEBGL_OUT="$OUT/WebGL~"; else WEBGL_OUT="$PKG/Runtime/StreamingAssets~"; fi

HOST="$(uname -s)"
want() { [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]; }
skip() { echo "skip $1: $2"; }
has_target() { rustup target list --installed | grep -qx "$1"; }

build() { # <target> [extra cargo args...]
    local target="$1"; shift
    (cd "$ROOT" && cargo build --manifest-path "$CRATE_DIR/Cargo.toml" --release --target "$target" "$@")
}

if want windows; then
    case "$HOST" in
        MINGW*|MSYS*|CYGWIN*)
            build x86_64-pc-windows-msvc
            mkdir -p "$PLUGINS/Windows/x86_64"
            cp "$TARGET_DIR/x86_64-pc-windows-msvc/release/hearth_unity.dll" "$PLUGINS/Windows/x86_64/"
            echo "built windows" ;;
        *) skip windows "needs a Windows host (MSVC toolchain)" ;;
    esac
fi

if want linux; then
    if [ "$HOST" = Linux ] && has_target x86_64-unknown-linux-gnu; then
        build x86_64-unknown-linux-gnu
        mkdir -p "$PLUGINS/Linux/x86_64"
        cp "$TARGET_DIR/x86_64-unknown-linux-gnu/release/libhearth_unity.so" "$PLUGINS/Linux/x86_64/"
        echo "built linux"
    else
        skip linux "needs a Linux host with x86_64-unknown-linux-gnu"
    fi
fi

if want macos; then
    if [ "$HOST" = Darwin ]; then
        build x86_64-apple-darwin
        build aarch64-apple-darwin
        mkdir -p "$PLUGINS/macOS"
        lipo -create \
            "$TARGET_DIR/x86_64-apple-darwin/release/libhearth_unity.dylib" \
            "$TARGET_DIR/aarch64-apple-darwin/release/libhearth_unity.dylib" \
            -output "$PLUGINS/macOS/hearth_unity.bundle"
        echo "built macos (universal)"
    else
        skip macos "needs a macOS host (lipo, Apple SDK)"
    fi
fi

if want ios; then
    if [ "$HOST" = Darwin ] && has_target aarch64-apple-ios; then
        build aarch64-apple-ios
        mkdir -p "$PLUGINS/iOS"
        cp "$TARGET_DIR/aarch64-apple-ios/release/libhearth_unity.a" "$PLUGINS/iOS/"
        echo "built ios"
    else
        skip ios "needs a macOS host with aarch64-apple-ios"
    fi
fi

if want android; then
    if command -v cargo-ndk >/dev/null && [ -n "${ANDROID_NDK_HOME:-}" ]; then
        (cd "$ROOT" && cargo ndk -t arm64-v8a -t armeabi-v7a -o "$PLUGINS/Android/libs" \
            build --manifest-path "$CRATE_DIR/Cargo.toml" --release)
        echo "built android"
    else
        skip android "needs cargo-ndk and ANDROID_NDK_HOME"
    fi
fi

if want webgl; then
    if has_target wasm32-unknown-unknown && command -v wasm-bindgen >/dev/null; then
        build wasm32-unknown-unknown --features webgl
        # The `~` suffix hides the folder from Unity's importer; WebGLCopyHook
        # copies it into StreamingAssets for WebGL builds.
        wasm-bindgen --target no-modules --out-name hearth_core --no-typescript \
            --out-dir "$WEBGL_OUT" \
            "$TARGET_DIR/wasm32-unknown-unknown/release/hearth_unity.wasm"
        echo "built webgl"
    else
        skip webgl "needs the wasm32-unknown-unknown target and wasm-bindgen-cli 0.2.129"
    fi
fi
