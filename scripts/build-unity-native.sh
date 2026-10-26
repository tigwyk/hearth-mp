#!/usr/bin/env bash
# Build the Hearth Unity native plugin for every platform this host can build,
# and copy the results into the UPM package's Plugins/ folder.
#
#   scripts/build-unity-native.sh                 # every buildable platform
#   scripts/build-unity-native.sh --only windows  # one of: windows linux macos android ios webgl
#
# Targets that cannot be built on this host are skipped with the reason.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG="$ROOT/unity/com.hearth.client"
CRATE=hearth-unity
TARGET_DIR="$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's#\\\\#/#g')"
ONLY=""

while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        *) echo "usage: $0 [--only windows|linux|macos|android|ios|webgl]" >&2; exit 2 ;;
    esac
done

HOST="$(uname -s)"
want() { [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]; }
skip() { echo "skip $1: $2"; }
has_target() { rustup target list --installed | grep -qx "$1"; }

build() { # <target> [extra cargo args...]
    local target="$1"; shift
    (cd "$ROOT" && cargo build -p "$CRATE" --release --target "$target" "$@")
}

if want windows; then
    case "$HOST" in
        MINGW*|MSYS*|CYGWIN*)
            build x86_64-pc-windows-msvc
            mkdir -p "$PKG/Plugins/Windows/x86_64"
            cp "$TARGET_DIR/x86_64-pc-windows-msvc/release/hearth_unity.dll" "$PKG/Plugins/Windows/x86_64/"
            echo "built windows" ;;
        *) skip windows "needs a Windows host (MSVC toolchain)" ;;
    esac
fi

if want linux; then
    if [ "$HOST" = Linux ] && has_target x86_64-unknown-linux-gnu; then
        build x86_64-unknown-linux-gnu
        mkdir -p "$PKG/Plugins/Linux/x86_64"
        cp "$TARGET_DIR/x86_64-unknown-linux-gnu/release/libhearth_unity.so" "$PKG/Plugins/Linux/x86_64/"
        echo "built linux"
    else
        skip linux "needs a Linux host with x86_64-unknown-linux-gnu"
    fi
fi

if want macos; then
    if [ "$HOST" = Darwin ]; then
        build x86_64-apple-darwin
        build aarch64-apple-darwin
        mkdir -p "$PKG/Plugins/macOS"
        lipo -create \
            "$TARGET_DIR/x86_64-apple-darwin/release/libhearth_unity.dylib" \
            "$TARGET_DIR/aarch64-apple-darwin/release/libhearth_unity.dylib" \
            -output "$PKG/Plugins/macOS/hearth_unity.bundle"
        echo "built macos (universal)"
    else
        skip macos "needs a macOS host (lipo, Apple SDK)"
    fi
fi

if want ios; then
    if [ "$HOST" = Darwin ] && has_target aarch64-apple-ios; then
        build aarch64-apple-ios
        mkdir -p "$PKG/Plugins/iOS"
        cp "$TARGET_DIR/aarch64-apple-ios/release/libhearth_unity.a" "$PKG/Plugins/iOS/"
        echo "built ios"
    else
        skip ios "needs a macOS host with aarch64-apple-ios"
    fi
fi

if want android; then
    if command -v cargo-ndk >/dev/null && [ -n "${ANDROID_NDK_HOME:-}" ]; then
        (cd "$ROOT" && cargo ndk -t arm64-v8a -t armeabi-v7a -o "$PKG/Plugins/Android/libs" build -p "$CRATE" --release)
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
            --out-dir "$PKG/Runtime/StreamingAssets~" \
            "$TARGET_DIR/wasm32-unknown-unknown/release/hearth_unity.wasm"
        echo "built webgl"
    else
        skip webgl "needs the wasm32-unknown-unknown target and wasm-bindgen-cli 0.2.129"
    fi
fi
