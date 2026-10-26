//! `hearth-conformance-server [--port 8790]`: see the crate docs. A local test
//! server only; it has no authentication. Never run it in production.

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let port = match args.iter().position(|a| a == "--port") {
        Some(i) => match args.get(i + 1).and_then(|p| p.parse().ok()) {
            Some(port) => port,
            None => {
                eprintln!("usage: hearth-conformance-server [--port <port>]");
                std::process::exit(2);
            }
        },
        None => 8790,
    };
    hearth_conformance_server::run(port)
}
