//! A fake dedicated game server for the fleet e2e.
//!
//! It is a normal process, like a real game server would be:
//! * reads its session env (`HEARTH_SID`, `HEARTH_SESSION_SECRET`,
//!   `HEARTH_REPORT_URL`) with the `hearth-mp-handoff-server` SDK,
//! * reports `started`,
//! * accepts `TICKET <ticket>` lines over TCP, redeeming each one with the SDK
//!   (the ticket is the credential; the secret is never sent to a client),
//! * after two players are in, reports `player_joined` for each, then a
//!   `result` (the first redeemed player wins) and `ended`.
//!
//! `--print-session` prints the session env as JSON and exits: the Task 4 Godot
//! helper uses it to learn the room/report URL of a session it was given.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use hearth_mp_handoff_server::{ServerReport, Session};
use hearth_mp_server::PlayerId;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn main() {
    if std::env::args().any(|arg| arg == "--print-session") {
        println!(
            "{}",
            serde_json::json!({
                "sid": env("HEARTH_SID"),
                "secret": env("HEARTH_SESSION_SECRET"),
                "report_url": env("HEARTH_REPORT_URL"),
            })
        );
        return;
    }

    let session = match Session::from_env() {
        Ok(session) => session,
        Err(error) => {
            eprintln!("fake-server: {error}");
            std::process::exit(2);
        }
    };
    let port: u16 = env("HEARTH_LISTEN_PORT").parse().unwrap_or(9300);
    if let Err(error) = session.report(&ServerReport::Started) {
        eprintln!("fake-server: reporting started failed: {error}");
    }
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("fake-server: bind {port} failed: {error}");
            std::process::exit(2);
        }
    };
    println!("LISTEN 127.0.0.1:{port}");
    let _ = std::io::stdout().flush();

    let mut redeemed: Vec<PlayerId> = Vec::new();
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let Ok(reader_stream) = stream.try_clone() else {
            continue;
        };
        let mut reader = BufReader::new(reader_stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let Some(ticket) = line.trim().strip_prefix("TICKET ") else {
            let _ = writeln!(stream, "ERR expected TICKET <ticket>");
            continue;
        };
        match session.redeem(ticket) {
            Ok(redeemed_player) => {
                let player = redeemed_player.player_id;
                println!("REDEEMED {}", player.0);
                let _ = writeln!(stream, "OK {}", player.0);
                redeemed.push(player);
                if redeemed.len() >= 2 {
                    for player in &redeemed {
                        if let Err(error) = session.report(&ServerReport::PlayerJoined {
                            player: player.clone(),
                        }) {
                            eprintln!("fake-server: player_joined failed: {error}");
                        }
                    }
                    let winner = redeemed.first().expect("two players").clone();
                    if let Err(error) = session.report(&ServerReport::Result {
                        data: serde_json::json!({ "winner": winner.0 }),
                    }) {
                        eprintln!("fake-server: result failed: {error}");
                    }
                    if let Err(error) = session.report(&ServerReport::Ended {
                        reason: "match over".to_owned(),
                    }) {
                        eprintln!("fake-server: ended failed: {error}");
                    }
                    println!("DONE");
                    return;
                }
            }
            Err(error) => {
                let _ = writeln!(stream, "ERR {error}");
            }
        }
    }
}
