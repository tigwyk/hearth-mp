#!/usr/bin/env bash
# One command for every v1 stability check (spec/stability.md):
#
#   scripts/check-stability.sh           # fail on any API change
#   scripts/check-stability.sh --bless   # accept the current API: rewrite api/
#
# - Rust: `cargo public-api` per crate vs api/<crate>.txt (needs a nightly
#   toolchain installed), and `cargo semver-checks` against the last release tag
#   (skipped, with a note, until a first tag exists).
# - C ABI: `cbindgen --verify` on the plugin header, and the header vs
#   api/hearth_unity.h.
# - C#: the Unity EditMode ApiSnapshotTests vs api/unity-csharp.txt
#   (UNITY = the Unity editor executable).
# - GDScript: tests/api_snapshot.gd vs api/godot-gdscript.txt
#   (GODOT = the Godot console executable).
# - The Wasm bundle budget (scripts/bundle-size.sh), last.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
BLESS=0
[ "${1:-}" = "--bless" ] && BLESS=1
FAILED=()
CRATES=(hearth-mp hearth-mp-protocol hearth-mp-sync hearth-mp-client hearth-mp-server hearth-mp-gateway hearth-mp-matchmaker
        hearth-mp-admin hearth-mp-test hearth-mp-unity-core hearth-mp-godot hearth-mp-handoff-server)
TMP="$(mktemp -d)"
mkdir -p api

changed() { FAILED+=("$1"); echo "CHANGED: $1"; }

# --- Rust public APIs ------------------------------------------------------
for crate in "${CRATES[@]}"; do
    if ! cargo public-api -p "$crate" --simplified > "$TMP/$crate.txt" 2> "$TMP/$crate.err"; then
        cat "$TMP/$crate.err" >&2
        changed "cargo public-api -p $crate failed"
        continue
    fi
    if [ "$BLESS" = 1 ]; then
        cp "$TMP/$crate.txt" "api/$crate.txt"
    elif ! diff -u --strip-trailing-cr "api/$crate.txt" "$TMP/$crate.txt" > "$TMP/$crate.diff" 2>&1; then
        cat "$TMP/$crate.diff"
        changed "Rust API of $crate"
    fi
done

# --- semver against the last release tag -----------------------------------
LAST_TAG="$(git describe --tags --abbrev=0 2>/dev/null || true)"
if [ -z "$LAST_TAG" ]; then
    echo "note: no release tag yet; cargo semver-checks skipped"
elif [ "$BLESS" = 0 ]; then
    for crate in "${CRATES[@]}"; do
        cargo semver-checks check-release -p "$crate" --baseline-rev "$LAST_TAG" || changed "semver-checks $crate vs $LAST_TAG"
    done
fi

# --- C ABI -------------------------------------------------------------------
HEADER=crates/hearth-mp-unity-plugin/include/hearth_unity.h
if ! (cd crates/hearth-mp-unity-plugin && RUSTC_BOOTSTRAP=1 cbindgen --config cbindgen.toml --output include/hearth_unity.h --verify >/dev/null 2>&1); then
    changed "C header out of date (regenerate: cd crates/hearth-mp-unity-plugin && RUSTC_BOOTSTRAP=1 cbindgen --config cbindgen.toml --output include/hearth_unity.h)"
fi
if [ "$BLESS" = 1 ]; then
    cp "$HEADER" api/hearth_unity.h
elif ! diff -u --strip-trailing-cr api/hearth_unity.h "$HEADER"; then
    changed "C ABI (api/hearth_unity.h)"
fi

# --- C# (Unity) ------------------------------------------------------------
UNITY="${UNITY:-/c/Program Files/Unity/Hub/Editor/6000.6.4f1/Editor/Unity.exe}"
if [ -x "$UNITY" ]; then
    PROJECT="$ROOT/examples/tictactoe-unity"
    command -v cygpath >/dev/null && PROJECT="$(cygpath -w "$PROJECT")"
    RESULTS="$TMP/unity-api.xml"
    command -v cygpath >/dev/null && RESULTS="$(cygpath -w "$RESULTS")"
    HEARTH_API_BLESS=$BLESS "$UNITY" -batchmode -nographics -projectPath "$PROJECT" -runTests -testPlatform EditMode \
        -testFilter ApiSnapshotTests -testResults "$RESULTS" -logFile - > "$TMP/unity.log" 2>&1
    if ! grep -q 'result="Passed"' "$TMP/unity-api.xml" 2>/dev/null; then
        grep -o 'API changed[^"<]*' "$TMP/unity-api.xml" 2>/dev/null | head -1
        changed "C# API (api/unity-csharp.txt)"
    fi
else
    changed "Unity not found: set UNITY to the editor executable"
fi

# --- GDScript (Godot) ------------------------------------------------------
GODOT="${GODOT:-$(command -v godot || echo "/c/Program Files/Godot/Godot_console.exe")}"
if [ -x "$GODOT" ]; then
    if [ -x examples/tictactoe-godot/scripts/sync-addon.ps1 ] || [ -f examples/tictactoe-godot/scripts/sync-addon.ps1 ]; then
        pwsh -NoProfile -File examples/tictactoe-godot/scripts/sync-addon.ps1 > /dev/null 2>&1 || true
    fi
    if ! HEARTH_API_BLESS=$BLESS "$GODOT" --headless --path examples/tictactoe-godot -s tests/api_snapshot.gd > "$TMP/godot.log" 2>&1; then
        grep -E "FAILED" "$TMP/godot.log"
        changed "GDScript API (api/godot-gdscript.txt)"
    fi
else
    changed "Godot not found: set GODOT to the console executable"
fi

# --- Bundle budget -----------------------------------------------------------
if [ -x scripts/bundle-size.sh ] || [ -f scripts/bundle-size.sh ]; then
    bash scripts/bundle-size.sh || changed "Wasm bundle budget (bundle-budget.toml)"
fi

rm -rf "$TMP"
if [ "${#FAILED[@]}" -gt 0 ]; then
    if [ "$BLESS" = 1 ]; then
        echo "check-stability --bless: could not finish: ${FAILED[*]}"
    else
        echo "API changed: review, then run scripts/check-stability.sh --bless"
    fi
    exit 1
fi
[ "$BLESS" = 1 ] && echo "check-stability: snapshots rewritten; read every api/ file before committing" || echo "check-stability: all stable"
