//! `hearth new`, `hearth codegen`, `hearth dev`, `hearth deploy`, `hearth
//! inspect` and `hearth cost`.
//!
//! Arguments are parsed with `std::env::args` (no clap). `cargo hearth ...`
//! invokes this binary as `cargo-hearth hearth ...`, so a leading `hearth`
//! argument is stripped first.

pub mod codegen;
pub mod cost;
pub mod inspect;

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{self, Command};

const USAGE: &str = "usage: hearth new <name> [--hearth-path <dir>] | \
hearth codegen --godot <out_dir> [--manifest-path <Cargo.toml>] | hearth dev | hearth deploy | \
hearth inspect rooms [--type T] [--all] | hearth inspect room <type>/<name> | \
hearth inspect dispose <type>/<name> [--yes] | hearth inspect tail <type>/<name> [--ops] | \
hearth cost --ccu <n> --room-size <n> --intents <n> [--ticked <hz>] [--json]";

const TEMPLATES: &[(&str, &str)] = &[
    ("Cargo.toml", include_str!("../templates/Cargo.toml.tmpl")),
    (
        "wrangler.toml",
        include_str!("../templates/wrangler.toml.tmpl"),
    ),
    ("src/lib.rs", include_str!("../templates/lib.rs.tmpl")),
    (".gitignore", include_str!("../templates/gitignore.tmpl")),
    (
        ".dev.vars.example",
        include_str!("../templates/dev.vars.example.tmpl"),
    ),
];

pub fn run() -> i32 {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("hearth") {
        args.remove(0);
    }
    let code = match args.first().map(String::as_str) {
        Some("new") => new_cmd(&args[1..]),
        Some("codegen") => codegen::run(&args[1..]),
        Some("dev") => wrangler_cmd("dev", &args[1..]),
        Some("deploy") => wrangler_cmd("deploy", &args[1..]),
        Some("inspect") => inspect::run(&args[1..]),
        Some("cost") => cost::run(&args[1..]),
        _ => usage(),
    };
    code
}

fn usage() -> i32 {
    eprintln!("{USAGE}");
    2
}

fn new_cmd(args: &[String]) -> i32 {
    let mut name: Option<&str> = None;
    let mut hearth_path: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--hearth-path" => match args.get(i + 1) {
                Some(dir) => {
                    hearth_path = Some(dir);
                    i += 2;
                }
                None => return usage(),
            },
            arg if arg.starts_with('-') => return usage(),
            arg => {
                if name.replace(arg).is_some() {
                    return usage();
                }
                i += 1;
            }
        }
    }
    let Some(name) = name else { return usage() };
    if !valid_name(name) {
        eprintln!(
            "invalid name: {name} (use ASCII letters, digits, '-' and '_', starting with a letter)"
        );
        return 2;
    }
    let dir = Path::new(name);
    if dir.exists() {
        eprintln!("{name} already exists; refusing to overwrite");
        return 1;
    }
    let hearth_dep = hearth_path.map(dep_path);
    if let Err(err) = write_project(dir, name, hearth_dep.as_deref()) {
        eprintln!("could not create {name}: {err}");
        return 1;
    }
    println!("created {name}");
    println!("next: cd {name} && hearth dev");
    0
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `my-game` becomes crate name `my_game`, room type `MyGame` and URL room
/// type `my-game`.
fn names(name: &str) -> (String, String, String) {
    let lower = name.to_ascii_lowercase();
    let crate_name = lower.replace(['-', '_'], "_");
    let room = lower
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    let room_type = lower.replace('_', "-");
    (crate_name, room, room_type)
}

/// Absolute, forward-slashed repo root for the generated `hearth` path dep.
fn dep_path(dir: &str) -> String {
    let path = Path::new(dir);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    absolute
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

fn write_project(dir: &Path, name: &str, hearth_dep: Option<&str>) -> io::Result<()> {
    let (crate_name, room, room_type) = names(name);
    fs::create_dir_all(dir.join("src"))?;
    for (relative, template) in TEMPLATES {
        let mut out = template
            .replace("{{name}}", name)
            .replace("{{crate_name}}", &crate_name)
            .replace("{{Room}}", &room)
            .replace("{{room_type}}", &room_type);
        if let Some(dep) = hearth_dep {
            out = out
                .replace(
                    "hearth = \"0.1\"",
                    &format!("hearth = {{ path = \"{dep}/crates/hearth\" }}"),
                )
                .replace(
                    "hearth-gateway = \"0.1\"",
                    &format!("hearth-gateway = {{ path = \"{dep}/crates/hearth-gateway\" }}"),
                );
        }
        fs::write(dir.join(relative), out)?;
    }
    Ok(())
}

fn wrangler_cmd(subcommand: &str, extra: &[String]) -> i32 {
    if subcommand == "deploy" {
        println!("reminder: run `wrangler secret put HEARTH_SECRET` once before your first deploy");
    }
    match run_wrangler(subcommand, extra) {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            eprintln!("wrangler not found; install it with: npm i -g wrangler");
            1
        }
        Err(err) => {
            eprintln!("could not run wrangler: {err}");
            1
        }
    }
}

fn run_wrangler(subcommand: &str, extra: &[String]) -> io::Result<process::ExitStatus> {
    let mut command = Command::new("wrangler");
    command.arg(subcommand).args(extra);
    match command.status() {
        // npm installs wrangler as wrangler.cmd on Windows.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Command::new("wrangler.cmd")
            .arg(subcommand)
            .args(extra)
            .status(),
        result => result,
    }
}
