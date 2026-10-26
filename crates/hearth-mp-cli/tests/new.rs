//! End-to-end checks for `hearth-mp new`. Hermetic: each test works in its own
//! temporary directory and drives the built binaries with `std::process::Command`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const HEARTH: &str = env!("CARGO_BIN_EXE_hearth-mp");
const CARGO_HEARTH: &str = env!("CARGO_BIN_EXE_cargo-hearth-mp");

const FILES: &[&str] = &[
    "Cargo.toml",
    "wrangler.toml",
    "src/lib.rs",
    ".gitignore",
    ".dev.vars.example",
];

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "hearth-cli-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/hearth-mp-cli is two levels below the repo root")
        .to_path_buf()
}

/// `leading` is empty for the `hearth-mp` binary and `["hearth-mp"]` for `cargo-hearth-mp`.
fn run_new(
    bin: &str,
    cwd: &Path,
    leading: &[&str],
    name: &str,
    hearth_path: Option<&Path>,
) -> Output {
    let mut command = Command::new(bin);
    command.args(leading).arg("new").arg(name);
    if let Some(path) = hearth_path {
        command.arg("--hearth-path").arg(path);
    }
    command
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn new_writes_the_templates_and_names_the_crate_and_room() {
    let temp = TempDir::new("names");
    let output = run_new(HEARTH, temp.path(), &[], "my-game", Some(&repo_root()));
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let root = temp.path().join("my-game");
    for file in FILES {
        assert!(root.join(file).is_file(), "missing {file}");
    }

    let cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("name = \"my_game\""), "{cargo}");
    assert!(cargo.contains("hearth-mp-gateway = { path = \""), "{cargo}");
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(lib.contains("pub struct MyGame;"), "{lib}");
    assert!(lib.contains("impl Room for MyGame"), "{lib}");
    assert!(lib.contains("#[event(fetch)]"), "{lib}");
    assert!(lib.contains("hearth_mp_gateway::serve"), "{lib}");
    assert!(lib.contains("RoomRoute::new("), "{lib}");
    assert!(lib.contains("\"my-game\","), "{lib}");
    assert!(lib.contains("\"ROOM\","), "{lib}");
    assert!(
        lib.contains("hearth_mp::server::schema_hash_hex::<MyGame>"),
        "{lib}"
    );
    let wrangler = fs::read_to_string(root.join("wrangler.toml")).unwrap();
    assert!(
        wrangler.contains("class_name = \"MyGameObject\""),
        "{wrangler}"
    );
    assert!(
        wrangler.contains("new_sqlite_classes = [\"MyGameObject\"]"),
        "{wrangler}"
    );
}

#[test]
fn new_refuses_an_existing_directory_and_leaves_it_untouched() {
    let temp = TempDir::new("refuse");
    let repo = repo_root();
    let first = run_new(HEARTH, temp.path(), &[], "my-game", Some(&repo));
    assert!(first.status.success(), "stderr: {}", stderr(&first));

    let root = temp.path().join("my-game");
    let before: Vec<(&str, Vec<u8>)> = FILES
        .iter()
        .map(|file| (*file, fs::read(root.join(file)).unwrap()))
        .collect();

    let second = run_new(HEARTH, temp.path(), &[], "my-game", Some(&repo));
    assert_eq!(second.status.code(), Some(1));
    assert!(
        stderr(&second).contains("my-game already exists; refusing to overwrite"),
        "stderr: {}",
        stderr(&second)
    );
    for (file, bytes) in before {
        assert_eq!(fs::read(root.join(file)).unwrap(), bytes, "{file} changed");
    }
}

#[test]
fn cargo_hearth_strips_the_leading_subcommand() {
    let temp = TempDir::new("cargo");
    let output = run_new(CARGO_HEARTH, temp.path(), &["hearth-mp"], "my-game", None);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let cargo = fs::read_to_string(temp.path().join("my-game").join("Cargo.toml")).unwrap();
    assert!(cargo.contains("hearth-mp = \"0.1\""), "{cargo}");
    assert!(cargo.contains("hearth-mp-gateway = \"0.1\""), "{cargo}");
    assert!(
        cargo.contains("crate-type = [\"cdylib\", \"rlib\"]"),
        "{cargo}"
    );
    let lib = fs::read_to_string(temp.path().join("my-game").join("src/lib.rs")).unwrap();
    assert!(lib.contains("#[event(fetch)]"), "{lib}");
}

#[test]
fn hearth_path_becomes_a_path_dependency() {
    let temp = TempDir::new("path");
    let output = run_new(HEARTH, temp.path(), &[], "pitched", Some(&repo_root()));
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let cargo = fs::read_to_string(temp.path().join("pitched").join("Cargo.toml")).unwrap();
    let repo = repo_root().display().to_string().replace('\\', "/");
    let expected = format!("hearth-mp = {{ path = \"{repo}/crates/hearth-mp\" }}");
    assert!(
        cargo.contains(&expected),
        "expected {expected:?} in:\n{cargo}"
    );
    let expected_gateway =
        format!("hearth-mp-gateway = {{ path = \"{repo}/crates/hearth-mp-gateway\" }}");
    assert!(
        cargo.contains(&expected_gateway),
        "expected {expected_gateway:?} in:\n{cargo}"
    );
    assert!(!cargo.contains("hearth-mp = \"0.1\""));
    assert!(!cargo.contains("hearth-mp-gateway = \"0.1\""));
}

#[test]
fn unknown_or_missing_command_prints_usage_and_exits_2() {
    let temp = TempDir::new("usage");
    for args in [vec!["frobnicate"], vec![]] {
        let output = Command::new(HEARTH)
            .args(&args)
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
        assert!(
            stderr(&output).contains("usage: hearth-mp new <name>"),
            "stderr: {}",
            stderr(&output)
        );
    }
}

#[test]
fn dev_without_wrangler_prints_the_install_hint() {
    let temp = TempDir::new("dev");
    let output = Command::new(HEARTH)
        .arg("dev")
        .current_dir(temp.path())
        .env("PATH", temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output).trim(),
        "wrangler not found; install it with: npm i -g wrangler"
    );
}
