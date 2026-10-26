//! cargo-hearth-mp entry point (so cargo hearth-mp ... works).

fn main() {
    std::process::exit(hearth_mp_cli::run());
}
