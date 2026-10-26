#!/usr/bin/env bash
# Wasm bundle sizes of the example Workers and a `hearth new` project, against
# bundle-budget.toml (gzip KiB per target). Exits non-zero when a target is over
# budget. A target without a budget entry is only reported.
#
#   scripts/bundle-size.sh
#
# Needs worker-build (cargo install worker-build) and gzip.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUDGET="$ROOT/bundle-budget.toml"
WORK="$(mktemp -d)"
STATUS=0

budget_for() {
    [ -f "$BUDGET" ] || return
    sed -n "s/^$1[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*/\1/p" "$BUDGET" | head -1
}

kib() { echo $(( ($1 + 1023) / 1024 )); }

measure() { # <name> <crate dir>
    local name="$1" dir="$2"
    if ! (cd "$dir" && worker-build --release > "$WORK/$name.log" 2>&1); then
        tail -20 "$WORK/$name.log"
        echo "$name: worker-build failed"
        STATUS=1
        return
    fi
    local raw=0 gz=0
    for file in "$dir"/build/*.wasm "$dir"/build/*.js "$dir"/build/worker/*.mjs; do
        [ -f "$file" ] || continue
        raw=$(( raw + $(wc -c < "$file") ))
        gz=$(( gz + $(gzip -9 -c "$file" | wc -c) ))
    done
    local budget
    budget="$(budget_for "$name")"
    local verdict="no budget"
    if [ -n "$budget" ]; then
        if [ "$(kib "$gz")" -le "$budget" ]; then verdict="ok (budget $budget KiB)"; else verdict="OVER (budget $budget KiB)"; STATUS=1; fi
    fi
    printf '%-16s raw %6s KiB  gzip %6s KiB  %s\n' "$name" "$(kib "$raw")" "$(kib "$gz")" "$verdict"
}

measure tictactoe "$ROOT/examples/tictactoe"
measure arena "$ROOT/examples/arena"
measure arena-dedicated "$ROOT/examples/arena-dedicated"

# A fresh `hearth new` project, built against this checkout.
(cd "$ROOT" && cargo build -q -p hearth-cli) || STATUS=1
HEARTH_BIN="$ROOT/target/debug/hearth"
[ -x "$HEARTH_BIN.exe" ] && HEARTH_BIN="$HEARTH_BIN.exe"
if (cd "$WORK" && "$HEARTH_BIN" new probe --hearth-path "$ROOT" > /dev/null); then
    measure hearth-new "$WORK/probe"
else
    echo "hearth-new: hearth new failed"
    STATUS=1
fi

rm -rf "$WORK"
exit $STATUS
