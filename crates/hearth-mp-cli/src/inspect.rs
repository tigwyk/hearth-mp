//! `hearth-mp inspect`: read a deployment's token-gated `/admin/*` JSON API.
//!
//! Blocking on purpose: a CLI command that ends when the server answers. The
//! four commands are `rooms`, `room`, `dispose` and `tail`; `docs/admin.md`
//! documents the API they speak.

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const DEFAULT_URL: &str = "http://127.0.0.1:8787";

const USAGE: &str = "usage: hearth-mp inspect rooms [--type T] [--all] | \
hearth-mp inspect room <type>/<name> | \
hearth-mp inspect dispose <type>/<name> [--yes] | \
hearth-mp inspect tail <type>/<name> [--ops]";

pub fn run(args: &[String]) -> i32 {
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => return usage(),
    };
    let flags = match Flags::parse(rest) {
        Ok(flags) => flags,
        Err(code) => return code,
    };
    let client = match Client::connect(flags.url.as_deref()) {
        Ok(client) => client,
        Err(code) => return code,
    };
    match command {
        "rooms" => rooms(&client, &flags),
        "room" => room(&client, &flags),
        "dispose" => dispose(&client, &flags),
        "tail" => tail(&client, &flags),
        _ => usage(),
    }
}

fn usage() -> i32 {
    eprintln!("{USAGE}");
    2
}

fn fail(msg: String) -> i32 {
    eprintln!("{msg}");
    1
}

#[derive(Default)]
struct Flags {
    url: Option<String>,
    room_type: Option<String>,
    all: bool,
    yes: bool,
    ops: bool,
    positional: Vec<String>,
}

impl Flags {
    fn parse(args: &[String]) -> Result<Self, i32> {
        let mut flags = Self::default();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--url" => flags.url = Some(take_value(args, &mut i)?),
                "--type" => flags.room_type = Some(take_value(args, &mut i)?),
                "--all" => {
                    flags.all = true;
                    i += 1;
                }
                "--yes" => {
                    flags.yes = true;
                    i += 1;
                }
                "--ops" => {
                    flags.ops = true;
                    i += 1;
                }
                arg if arg.starts_with('-') => return Err(usage()),
                arg => {
                    flags.positional.push(arg.to_owned());
                    i += 1;
                }
            }
        }
        Ok(flags)
    }
}

fn take_value(args: &[String], i: &mut usize) -> Result<String, i32> {
    match args.get(*i + 1) {
        Some(value) => {
            *i += 2;
            Ok(value.clone())
        }
        None => Err(usage()),
    }
}

struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Client {
    /// Resolve `--url` / `HEARTH_URL` / the dev default and the admin token.
    fn connect(url: Option<&str>) -> Result<Self, i32> {
        let base = url
            .map(str::to_owned)
            .or_else(|| env::var("HEARTH_URL").ok().filter(|url| !url.is_empty()))
            .unwrap_or_else(|| DEFAULT_URL.to_owned());
        let base = base.trim_end_matches('/').to_owned();
        let token = env::var("HEARTH_ADMIN_TOKEN")
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .or_else(|| local_dev_token(&base));
        let Some(token) = token else {
            eprintln!("set HEARTH_ADMIN_TOKEN (see docs/admin.md)");
            return Err(2);
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self { base, token, agent })
    }

    fn get(&self, path: &str) -> Result<(u16, String), String> {
        let response = self
            .agent
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()
            .map_err(|err| format!("request to {} failed: {err}", self.base))?;
        finish(response)
    }

    fn post(&self, path: &str) -> Result<(u16, String), String> {
        let response = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .send_empty()
            .map_err(|err| format!("request to {} failed: {err}", self.base))?;
        finish(response)
    }

    fn get_ok(&self, path: &str, target: Target) -> Result<String, i32> {
        let (status, body) = self.get(path).map_err(fail)?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(fail(self.error(status, &body, target)))
        }
    }

    fn post_ok(&self, path: &str, target: Target) -> Result<(), i32> {
        let (status, body) = self.post(path).map_err(fail)?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(fail(self.error(status, &body, target)))
        }
    }

    fn error(&self, status: u16, body: &str, target: Target) -> String {
        match status {
            401 => "admin token rejected".to_owned(),
            404 if target == Target::List => {
                "admin API disabled on this deployment (HEARTH_ADMIN_TOKEN not set there)"
                    .to_owned()
            }
            404 => "no such room".to_owned(),
            501 => error_field(body).unwrap_or_else(|| format!("HTTP {status}: {}", body.trim())),
            _ => format!("HTTP {status}: {}", body.trim()),
        }
    }
}

fn finish(mut response: ureq::http::Response<ureq::Body>) -> Result<(u16, String), String> {
    let status = response.status().as_u16();
    // 204 and 304 are bodyless by definition; reading them upsets the gzip
    // decoder on some servers.
    if matches!(status, 204 | 205 | 304) {
        return Ok((status, String::new()));
    }
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|err| format!("could not read the response body: {err}"))?;
    Ok((status, body))
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    List,
    Room,
}

/// The server's `{"error": ...}` body, when it sent one.
fn error_field(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_owned)
}

fn rooms(client: &Client, flags: &Flags) -> i32 {
    if !flags.positional.is_empty() {
        return usage();
    }
    let mut rooms: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = rooms_path(flags.room_type.as_deref(), cursor.as_deref(), flags.all);
        let body = match client.get_ok(&path, Target::List) {
            Ok(body) => body,
            Err(code) => return code,
        };
        let page: Value = match serde_json::from_str(&body) {
            Ok(page) => page,
            Err(err) => return fail(format!("unreadable /admin/rooms response: {err}")),
        };
        if let Some(list) = page.get("rooms").and_then(Value::as_array) {
            rooms.extend(list.iter().cloned());
        }
        // The server hands out one opaque cursor string and takes it back
        // verbatim; a JSON `RoomName` cursor is also accepted for older servers.
        cursor = match page.get("next") {
            Some(Value::String(next)) if flags.all && !next.is_empty() => Some(next.clone()),
            Some(next) if flags.all && !next.is_null() => serde_json::to_string(next)
                .ok()
                .map(|json| base64url(json.as_bytes())),
            _ => None,
        };
        if cursor.is_none() {
            break;
        }
    }
    print_rooms(&rooms);
    0
}

fn rooms_path(room_type: Option<&str>, cursor: Option<&str>, all: bool) -> String {
    let mut params: Vec<String> = Vec::new();
    if let Some(room_type) = room_type {
        params.push(format!("type={room_type}"));
    }
    if let Some(cursor) = cursor {
        params.push(format!("cursor={cursor}"));
    }
    if all {
        params.push("limit=100".to_owned());
    }
    if params.is_empty() {
        "/admin/rooms".to_owned()
    } else {
        format!("/admin/rooms?{}", params.join("&"))
    }
}

fn print_rooms(rooms: &[Value]) {
    let now = now_ms();
    let header = ["TYPE", "NAME", "PLAYERS", "AGE"];
    let rows: Vec<[String; 4]> = rooms
        .iter()
        .map(|room| {
            let created_ms = room
                .get("created_ms")
                .and_then(Value::as_u64)
                .unwrap_or(now);
            [
                str_field(room, "room_type").to_owned(),
                str_field(room, "name").to_owned(),
                room.get("players")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .to_string(),
                age(now.saturating_sub(created_ms)),
            ]
        })
        .collect();
    let mut widths = header.map(str::len);
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    print_row(&header.map(str::to_owned), &widths);
    for row in &rows {
        print_row(row, &widths);
    }
}

fn print_row(cells: &[String; 4], widths: &[usize; 4]) {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        line.push_str(&format!("{cell:<width$}", width = widths[i]));
    }
    println!("{}", line.trim_end());
}

fn age(age_ms: u64) -> String {
    let secs = age_ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn room(client: &Client, flags: &Flags) -> i32 {
    let (room_type, name) = match room_arg(flags) {
        Ok(room) => room,
        Err(code) => return code,
    };
    let path = format!("/admin/rooms/{room_type}/{name}");
    let body = match client.get_ok(&path, Target::Room) {
        Ok(body) => body,
        Err(code) => return code,
    };
    match serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|doc| serde_json::to_string_pretty(&doc).ok())
    {
        Some(pretty) => println!("{pretty}"),
        None => println!("{body}"),
    }
    0
}

fn dispose(client: &Client, flags: &Flags) -> i32 {
    let (room_type, name) = match room_arg(flags) {
        Ok(room) => room,
        Err(code) => return code,
    };
    if !flags.yes {
        print!(
            "Dispose {room_type}/{name}? This wipes its storage and disconnects everyone. [y/N] "
        );
        if io::stdout().flush().is_err() || !confirm(io::stdin().lock()) {
            eprintln!("aborted");
            return 1;
        }
    }
    let path = format!("/admin/rooms/{room_type}/{name}/dispose");
    match client.post_ok(&path, Target::Room) {
        Ok(()) => {
            println!("disposed {room_type}/{name}");
            0
        }
        Err(code) => code,
    }
}

fn tail(client: &Client, flags: &Flags) -> i32 {
    let (room_type, name) = match room_arg(flags) {
        Ok(room) => room,
        Err(code) => return code,
    };
    let path = format!("/admin/rooms/{room_type}/{name}/tail");
    let url = ws_url(&client.base, &path);
    let uri: tungstenite::http::Uri = match url.parse() {
        Ok(uri) => uri,
        Err(err) => return fail(format!("bad tail URL {url}: {err}")),
    };
    let request = tungstenite::ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {}", client.token));
    let (mut socket, _) = match tungstenite::connect(request) {
        Ok(connected) => connected,
        Err(err) => return fail(tail_error(err)),
    };
    loop {
        let message = match socket.read() {
            Ok(message) => message,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return 0;
            }
            Err(err) => return fail(format!("tail failed: {err}")),
        };
        match message {
            tungstenite::Message::Text(text) => print_tail_line(text.as_str(), flags.ops),
            tungstenite::Message::Close(_) => return 0,
            _ => {}
        }
    }
}

fn print_tail_line(text: &str, with_ops: bool) {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        println!("{text}");
        return;
    };
    let summary = summarize(&msg);
    match (with_ops, msg.get("ops")) {
        (true, Some(ops)) => println!("{summary} {ops}"),
        _ => println!("{summary}"),
    }
}

fn tail_error(err: tungstenite::Error) -> String {
    if let tungstenite::Error::Http(response) = &err {
        return match response.status().as_u16() {
            401 => "admin token rejected".to_owned(),
            404 => "no such room".to_owned(),
            status => format!("HTTP {status}"),
        };
    }
    format!("tail connection failed: {err}")
}

fn ws_url(base: &str, path: &str) -> String {
    let base = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .unwrap_or_else(|| base.to_owned());
    format!("{base}{path}")
}

fn room_arg(flags: &Flags) -> Result<(String, String), i32> {
    match flags.positional.as_slice() {
        [arg] => parse_room(arg).map_err(|err| {
            eprintln!("{err}");
            2
        }),
        _ => Err(usage()),
    }
}

/// The token from `.dev.vars` in the current directory, for a local URL only: a
/// deployment's token must never come from a file on disk.
fn local_dev_token(base: &str) -> Option<String> {
    if !is_localhost(base) {
        return None;
    }
    let vars = fs::read_to_string(".dev.vars").ok()?;
    vars.lines()
        .filter_map(|line| line.trim().split_once('='))
        .find(|(key, _)| key.trim() == "HEARTH_ADMIN_TOKEN")
        .map(|(_, value)| value.trim().trim_matches(['"', '\'']).to_owned())
        .filter(|token| !token.is_empty())
}

fn is_localhost(base: &str) -> bool {
    let authority = base
        .split_once("://")
        .map_or(base, |(_, authority)| authority);
    let host = authority.split('/').next().unwrap_or(authority);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host.rsplit_once(':').map_or(host, |(host, _)| host),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("-")
}

/// base64url without padding of the JSON `RoomName`: the opaque cursor the admin
/// API hands out as `next` and takes back as `cursor`.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut bits = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            bits |= u32::from(*byte) << (16 - 8 * i);
        }
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(bits >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    out
}

/// Split a `<type>/<name>` argument: exactly one `/`, both parts non-empty.
pub fn parse_room(arg: &str) -> Result<(String, String), String> {
    let mut parts = arg.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(room_type), Some(name), None) if !room_type.is_empty() && !name.is_empty() => {
            Ok((room_type.to_owned(), name.to_owned()))
        }
        _ => Err(format!("expected <type>/<name>, got {arg:?}")),
    }
}

/// One `TailMsg` as one line: `#4 intent a#1 3 ops`.
pub fn summarize(msg: &Value) -> String {
    let seq = msg.get("seq").and_then(Value::as_u64).unwrap_or(0);
    let causes: Vec<String> = msg
        .get("cause")
        .and_then(Value::as_array)
        .map(|causes| causes.iter().map(cause_summary).collect())
        .unwrap_or_default();
    let causes = if causes.is_empty() {
        "?".to_owned()
    } else {
        causes.join(", ")
    };
    let ops = msg.get("ops").and_then(Value::as_array).map_or(0, Vec::len);
    format!("#{seq} {causes} {ops} ops")
}

fn cause_summary(cause: &Value) -> String {
    let kind = cause.get("cause").and_then(Value::as_str).unwrap_or("?");
    match kind {
        "intent" => format!(
            "intent {}#{}",
            str_field(cause, "player"),
            cause.get("intent_seq").and_then(Value::as_u64).unwrap_or(0)
        ),
        "timer" => format!("timer {}", str_field(cause, "key")),
        "dispose" => "dispose".to_owned(),
        other => match cause.get("player").and_then(Value::as_str) {
            Some(player) => format!("{other} {player}"),
            None => other.to_owned(),
        },
    }
}

/// Read one answer from `reader`; only `y` or `yes` (case-insensitive) proceed.
pub fn confirm(reader: impl BufRead) -> bool {
    let mut reader = reader;
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return false;
    }
    let answer = line.trim().to_ascii_lowercase();
    answer == "y" || answer == "yes"
}

#[cfg(test)]
mod tests {
    use super::base64url;

    #[test]
    fn cursor_encoding_is_base64url_without_padding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"a"), "YQ");
        assert_eq!(base64url(b"ab"), "YWI");
        assert_eq!(base64url(b"abc"), "YWJj");
        assert_eq!(
            base64url(br#"{"room_type":"t","name":"n"}"#),
            "eyJyb29tX3R5cGUiOiJ0IiwibmFtZSI6Im4ifQ"
        );
    }
}
