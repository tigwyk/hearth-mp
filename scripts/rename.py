"""Rename the Rust side of Hearth to Hearth MP (docs/release/naming.md).

    python scripts/rename.py --dry-run   # list every change, touch nothing
    python scripts/rename.py             # move the crate directories, rewrite files

It rewrites only exact crate names, never a bare "hearth": `hearth-<crate>` ->
`hearth-mp-<crate>`, `hearth_<crate>` -> `hearth_mp_<crate>`, the facade
`hearth` crate (`hearth::`, Cargo keys, `crates/hearth`), and the CLI binaries
(`hearth-mp`, `cargo-hearth-mp`). The frozen wire strings in
scripts/wire-constants.txt are never crate names, so no rule can match them;
crates/hearth-mp-protocol/tests/wire_constants.rs checks that afterwards.
"""

import re
import subprocess
import sys

DRY = "--dry-run" in sys.argv

CRATES = [  # longest first, so `server-macros` wins over `server`
    "conformance-server", "handoff-server", "server-macros", "sync-derive",
    "unity-plugin", "unity-core", "matchmaker", "protocol", "gateway",
    "client", "server", "godot", "admin", "sync", "test", "cli",
]
# `hearth_unity` (the plugin's lib name) is the C ABI and stays.
SNAKE = [c.replace("-", "_") for c in CRATES if c != "unity-plugin"]

RULES = [
    # hearth-protocol -> hearth-mp-protocol (crate names, directories, api/ files)
    (re.compile(r"(?<![\w-])hearth-(" + "|".join(CRATES) + r")(?![\w-])"), r"hearth-mp-\1"),
    # hearth_protocol -> hearth_mp_protocol (Rust paths, lib file names incl. libhearth_godot.so)
    (re.compile(r"(?:(?<=\blib)|(?<!\w))hearth_(" + "|".join(SNAKE) + r")(?!\w)(?!\.gd)"), r"hearth_mp_\1"),
    # the facade crate
    (re.compile(r"(?<!\w)hearth::"), "hearth_mp::"),
    (re.compile(r"^(\s*)hearth(\s*=|\.workspace)", re.M), r"\1hearth-mp\2"),
    (re.compile(r'^name = "hearth"$', re.M), 'name = "hearth-mp"'),
    (re.compile(r'"hearth/'), '"hearth-mp/'),
    (re.compile(r'crate = (\\?)"hearth\1"'), r'crate = \1"hearth_mp\1"'),
    (re.compile(r"crates/hearth(?![\w-])"), "crates/hearth-mp"),
    (re.compile(r"-p hearth(?![\w-])"), "-p hearth-mp"),
    # the CLI binaries
    (re.compile(r"CARGO_BIN_EXE_(cargo-)?hearth(?![\w-])"), r"CARGO_BIN_EXE_\1hearth-mp"),
    (re.compile(r"cargo-hearth(?![\w-])"), "cargo-hearth-mp"),
    (re.compile(r"cargo hearth(?![\w-])"), "cargo hearth-mp"),
    (re.compile(r"src/bin/hearth\.rs"), "src/bin/hearth-mp.rs"),
    (re.compile(r"(?<![\w/.:-])hearth (?=(?:new|codegen|dev|deploy|inspect|cost)\b)"), "hearth-mp "),
]

SKIP_PREFIXES = ("api/", "docs/superpowers/", "docs/release/naming.md", ".recon/", "scripts/rename.py",
                 "scripts/wire-constants.txt", "Hearth a Rust framework")
TEXT = (".rs", ".toml", ".md", ".sh", ".ps1", ".mjs", ".js", ".json", ".gd", ".cs", ".cfg", ".tmpl",
        ".gdextension", ".h", ".txt", ".yml", ".yaml", ".asmdef", ".example", ".tscn", ".godot")


def tracked():
    out = subprocess.run(["git", "ls-files"], capture_output=True, text=True, check=True).stdout
    return [f for f in out.splitlines() if f]


def main():
    files = tracked()
    total = 0
    for path in files:
        if path.startswith(SKIP_PREFIXES) or not path.endswith(TEXT) or path == "Cargo.lock":
            continue
        try:
            text = open(path, encoding="utf-8", newline="").read()
        except (UnicodeDecodeError, FileNotFoundError):
            continue
        new = text
        for pattern, repl in RULES:
            new = pattern.sub(repl, new)
        if new == text:
            continue
        changed = [(a, b) for a, b in zip(text.splitlines(), new.splitlines()) if a != b]
        total += len(changed)
        print(f"{path}: {len(changed)} lines")
        if DRY:
            for a, b in changed[:3]:
                print(f"    - {a.strip()[:110]}\n    + {b.strip()[:110]}")
        else:
            open(path, "w", encoding="utf-8", newline="").write(new)

    moves = [("crates/hearth", "crates/hearth-mp")]
    moves += [(f"crates/hearth-{c}", f"crates/hearth-mp-{c}") for c in CRATES]
    moves += [("crates/hearth-mp-cli/src/bin/hearth.rs", "crates/hearth-mp-cli/src/bin/hearth-mp.rs"),
              ("crates/hearth-mp-cli/src/bin/cargo-hearth.rs", "crates/hearth-mp-cli/src/bin/cargo-hearth-mp.rs")]
    for old, new in moves:
        print(f"move {old} -> {new}")
        if not DRY:
            subprocess.run(["git", "mv", old, new], check=True)
    print(f"{total} changed lines" + (" (dry run)" if DRY else ""))


if __name__ == "__main__":
    main()
