//! `hearth cost`: estimate a deployment's monthly Cloudflare bill from a
//! versioned pricing table.
//!
//! Offline by design: it is a pure function over `pricing.toml`, the inputs on
//! the command line and the model in [`quantities`]. Nothing here talks to
//! Cloudflare or reads an account.

use std::fs;

use serde::{Deserialize, Serialize};

/// The shipped pricing table, embedded so the binary works without the repo.
pub const PRICING_TOML: &str = include_str!("../pricing.toml");

const USAGE: &str = "usage: hearth cost --ccu <n> --room-size <n> --intents <n> \
[--ticked <hz>] [--persist-every <ticks>] [--state-kb <kb>] [--sessions <n>] \
[--pricing <file>] [--json]";

/// The built-in pricing table. A malformed `pricing.toml` is a build-time bug:
/// `tests/cost.rs::embedded_pricing_parses_and_has_as_of` fails before shipping.
pub fn embedded_pricing() -> Pricing {
    toml::from_str(PRICING_TOML).expect("embedded pricing.toml is valid")
}

/// `pricing.toml`, mirrored. The file is the source of truth; this type only
/// gives it names.
#[derive(Deserialize)]
pub struct Pricing {
    pub as_of: String,
    pub sources: Vec<String>,
    pub workers: Workers,
    pub durable_objects: DurableObjects,
    pub model: Model,
}

#[derive(Deserialize)]
pub struct Workers {
    pub base_usd_month: f64,
    pub requests_included: f64,
    pub usd_per_million_requests: f64,
}

#[derive(Deserialize)]
pub struct DurableObjects {
    pub requests_included: f64,
    pub usd_per_million_requests: f64,
    pub gb_s_included: f64,
    pub usd_per_million_gb_s: f64,
    pub memory_gb: f64,
    pub ws_incoming_ratio: f64,
    pub rows_written_included: f64,
    pub usd_per_million_rows_written: f64,
    pub storage_gb_included: f64,
    pub usd_per_gb_month: f64,
}

#[derive(Deserialize)]
pub struct Model {
    pub seconds_per_month: f64,
    pub idle_hibernate_after_s: f64,
}

/// How a room is driven. `Ticked` is the shipped v0.4 runtime: an in-object loop
/// keeps the room awake while it ticks, so a tick is a duration cost, not a
/// request; one watchdog alarm per five minutes renews that loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CostMode {
    Event,
    Ticked { hz: f64, persist_every_ticks: u32 },
}

/// One estimate run: the numbers the dev knows about their game.
#[derive(Clone, Debug)]
pub struct CostInput {
    pub ccu: f64,
    pub room_size: f64,
    pub intents_per_sec: f64,
    pub mode: CostMode,
    pub state_kb: f64,
    pub sessions_per_player_day: f64,
}

/// One metered, billable dimension.
#[derive(Serialize, Clone, Debug)]
pub struct LineItem {
    pub name: &'static str,
    pub quantity: f64,
    pub unit: &'static str,
    pub billable: f64,
    pub usd: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Estimate {
    pub items: Vec<LineItem>,
    pub total_usd: f64,
    pub assumptions: Vec<String>,
}

/// Every intermediate value of the model, so a test (or a curious reader) can
/// check one step at a time instead of only the final bill.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quantities {
    pub rooms: f64,
    pub msgs_per_player: f64,
    pub ws_msgs: f64,
    pub ticks: f64,
    pub watchdog_alarms: f64,
    pub connects: f64,
    pub do_requests: f64,
    pub worker_requests: f64,
    pub room_msg_rate: f64,
    pub active_fraction: f64,
    pub gb_s: f64,
    pub rows_written: f64,
    pub storage_gb: f64,
}

/// The model, step by step. Everything below is a monthly number.
pub fn quantities(input: &CostInput, pricing: &Pricing) -> Quantities {
    let s = pricing.model.seconds_per_month;
    let rooms = input.ccu / input.room_size;
    // The SDK batches intents; more than one intent per 50 ms does not add
    // frames, so the frame rate is capped at 20/s per player.
    let msgs_per_player = input.intents_per_sec.min(20.0);
    let ws_msgs = input.ccu * msgs_per_player * s;
    let ticks = match input.mode {
        CostMode::Event => 0.0,
        CostMode::Ticked { hz, .. } => rooms * hz * s,
    };
    // The v0.4 adapter drives ticks from an in-object loop, so a tick is not a
    // Durable Object request. One watchdog alarm per five minutes renews that
    // loop (and recovers it after an eviction); see ADR 0001. The loop keeps the
    // object in memory, which the duration charge below already models.
    let watchdog_alarms = match input.mode {
        CostMode::Event => 0.0,
        CostMode::Ticked { .. } => rooms * (s / 300.0),
    };
    // One auth call, then one WebSocket upgrade per session.
    let connects = input.ccu * input.sessions_per_player_day * 30.0;
    let do_requests =
        ws_msgs / pricing.durable_objects.ws_incoming_ratio + connects + watchdog_alarms;
    let worker_requests = connects + connects;
    let room_msg_rate = input.room_size * msgs_per_player;
    let active_fraction = match input.mode {
        // A room is out of memory (and unbilled) once it has been idle for
        // `idle_hibernate_after_s`; a busy room is awake all month.
        CostMode::Event => (room_msg_rate * pricing.model.idle_hibernate_after_s).min(1.0),
        CostMode::Ticked { .. } => 1.0,
    };
    let gb_s = rooms * active_fraction * s * pricing.durable_objects.memory_gb;
    let rows_written = match input.mode {
        CostMode::Event => rooms * room_msg_rate * s,
        CostMode::Ticked {
            hz,
            persist_every_ticks,
        } => rooms * (hz / f64::from(persist_every_ticks.max(1))) * s,
    };
    let storage_gb = rooms * input.state_kb / 1e6;
    Quantities {
        rooms,
        msgs_per_player,
        ws_msgs,
        ticks,
        watchdog_alarms,
        connects,
        do_requests,
        worker_requests,
        room_msg_rate,
        active_fraction,
        gb_s,
        rows_written,
        storage_gb,
    }
}

/// The monthly estimate. `total_usd` includes the Workers Paid plan minimum.
pub fn estimate(input: &CostInput, pricing: &Pricing) -> Estimate {
    let q = quantities(input, pricing);
    let bo = &pricing.durable_objects;
    let workers_requests = billable(q.worker_requests, pricing.workers.requests_included);
    let do_requests = billable(q.do_requests, bo.requests_included);
    let gb_s = billable(q.gb_s, bo.gb_s_included);
    let rows_written = billable(q.rows_written, bo.rows_written_included);
    let storage_gb = billable(q.storage_gb, bo.storage_gb_included);
    let items = vec![
        LineItem {
            name: "workers requests",
            quantity: q.worker_requests,
            unit: "requests",
            billable: workers_requests,
            usd: per_million(workers_requests, pricing.workers.usd_per_million_requests),
        },
        LineItem {
            name: "durable objects requests",
            quantity: q.do_requests,
            unit: "requests",
            billable: do_requests,
            usd: per_million(do_requests, bo.usd_per_million_requests),
        },
        LineItem {
            name: "durable objects duration",
            quantity: q.gb_s,
            unit: "GB-s",
            billable: gb_s,
            usd: per_million(gb_s, bo.usd_per_million_gb_s),
        },
        LineItem {
            name: "durable objects rows written",
            quantity: q.rows_written,
            unit: "rows",
            billable: rows_written,
            usd: per_million(rows_written, bo.usd_per_million_rows_written),
        },
        LineItem {
            name: "durable objects storage",
            quantity: q.storage_gb,
            unit: "GB-month",
            billable: storage_gb,
            usd: storage_gb * bo.usd_per_gb_month,
        },
    ];
    let total_usd = pricing.workers.base_usd_month + items.iter().map(|item| item.usd).sum::<f64>();
    Estimate {
        items,
        total_usd,
        assumptions: assumptions(input, pricing, &q),
    }
}

fn assumptions(input: &CostInput, pricing: &Pricing, q: &Quantities) -> Vec<String> {
    let mut assumptions = vec![
        format!(
            "S = {:.0} seconds per month (30.4-day average month)",
            pricing.model.seconds_per_month
        ),
        format!(
            "rooms = ccu / room_size = {:.0} / {:.0} = {:.2}",
            input.ccu, input.room_size, q.rooms
        ),
        format!(
            "msgs_per_player = min(intents_per_sec, 20) = {:.2} frames/s (SDK batching cap)",
            q.msgs_per_player
        ),
        format!(
            "ws_msgs = ccu * msgs_per_player * S = {:.0} incoming WebSocket messages",
            q.ws_msgs
        ),
        format!(
            "connects = ccu * sessions_per_player_day * 30 = {:.0} sessions",
            q.connects
        ),
        format!(
            "do_requests = ws_msgs / {:.0} + connects + watchdog_alarms = {:.0}",
            pricing.durable_objects.ws_incoming_ratio, q.do_requests
        ),
        format!(
            "worker_requests = connects * 2 = {:.0} (auth call + WebSocket upgrade per session)",
            q.worker_requests
        ),
        format!(
            "room_msg_rate = room_size * msgs_per_player = {:.2} frames/s per room",
            q.room_msg_rate
        ),
        format!(
            "active_fraction = {:.4} ({} mode)",
            q.active_fraction,
            match input.mode {
                CostMode::Event => "event",
                CostMode::Ticked { .. } => "ticked",
            }
        ),
        format!(
            "gb_s = rooms * active_fraction * S * {} GB = {:.0} GB-s",
            pricing.durable_objects.memory_gb, q.gb_s
        ),
        format!(
            "rows_written = {} = {:.0} rows",
            match input.mode {
                CostMode::Event => "rooms * room_msg_rate * S".to_owned(),
                CostMode::Ticked {
                    hz,
                    persist_every_ticks,
                } => format!("rooms * ({hz} / {persist_every_ticks}) * S"),
            },
            q.rows_written
        ),
        format!(
            "storage_gb = rooms * state_kb / 1000000 = {:.6} GB",
            q.storage_gb
        ),
        format!(
            "Workers Paid plan base ${:.2}/month is included in the total",
            pricing.workers.base_usd_month
        ),
        "Cloudflare rounds each billable unit up before charging; this estimate does not round, so small overages are understated.".to_owned(),
        "Not modeled: Worker CPU time, Durable Object rows read, KV/D1/R2, and game-client hosting. Egress from Workers is free.".to_owned(),
    ];
    if let CostMode::Ticked {
        hz,
        persist_every_ticks,
    } = input.mode
    {
        assumptions.push(format!(
            "ticks = rooms * {hz} Hz * S = {:.0} simulated ticks (loop driver: duration, not requests)",
            q.ticks
        ));
        assumptions.push(format!(
            "watchdog_alarms = rooms * S / 300 = {:.0} alarms (one per room per five minutes)",
            q.watchdog_alarms
        ));
        assumptions.push(format!(
            "persist_every_ticks = {persist_every_ticks} (one state row per {persist_every_ticks} ticks)"
        ));
    }
    assumptions
}

fn billable(quantity: f64, included: f64) -> f64 {
    (quantity - included).max(0.0)
}

fn per_million(billable: f64, usd_per_million: f64) -> f64 {
    billable / 1e6 * usd_per_million
}

pub fn run(args: &[String]) -> i32 {
    let mut ccu: Option<f64> = None;
    let mut room_size: Option<f64> = None;
    let mut intents: Option<f64> = None;
    let mut ticked: Option<f64> = None;
    let mut persist_every: Option<u32> = None;
    let mut state_kb: f64 = 4.0;
    let mut sessions: f64 = 2.0;
    let mut pricing_path: Option<String> = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ccu" => {
                ccu = Some(match number(args, &mut i, "--ccu") {
                    Ok(value) => value,
                    Err(code) => return code,
                })
            }
            "--room-size" => {
                room_size = Some(match number(args, &mut i, "--room-size") {
                    Ok(value) => value,
                    Err(code) => return code,
                })
            }
            "--intents" => {
                intents = Some(match number(args, &mut i, "--intents") {
                    Ok(value) => value,
                    Err(code) => return code,
                })
            }
            "--ticked" => {
                ticked = Some(match number(args, &mut i, "--ticked") {
                    Ok(value) => value,
                    Err(code) => return code,
                })
            }
            "--persist-every" => {
                persist_every = Some(match integer(args, &mut i, "--persist-every") {
                    Ok(value) => value,
                    Err(code) => return code,
                })
            }
            "--state-kb" => {
                state_kb = match number(args, &mut i, "--state-kb") {
                    Ok(value) => value,
                    Err(code) => return code,
                }
            }
            "--sessions" => {
                sessions = match number(args, &mut i, "--sessions") {
                    Ok(value) => value,
                    Err(code) => return code,
                }
            }
            "--pricing" => match args.get(i + 1) {
                Some(path) => {
                    pricing_path = Some(path.clone());
                    i += 2;
                }
                None => return usage_error("--pricing needs a file path"),
            },
            "--json" => {
                json = true;
                i += 1;
            }
            arg => return usage_error(format!("unexpected argument {arg:?}")),
        }
    }

    let ccu = match positive(ccu, "--ccu") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let room_size = match positive(room_size, "--room-size") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let intents = match nonnegative(intents, "--intents") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let state_kb = match positive(Some(state_kb), "--state-kb") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let sessions = match positive(Some(sessions), "--sessions") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let mode = match ticked {
        Some(hz) => {
            let hz = match positive(Some(hz), "--ticked") {
                Ok(value) => value,
                Err(code) => return code,
            };
            let persist_every_ticks = match persist_every {
                Some(0) => return usage_error("--persist-every must be at least 1"),
                Some(ticks) => ticks,
                None => 20,
            };
            CostMode::Ticked {
                hz,
                persist_every_ticks,
            }
        }
        None => {
            if persist_every.is_some() {
                return usage_error("--persist-every requires --ticked");
            }
            CostMode::Event
        }
    };
    let pricing = match pricing_path {
        Some(path) => match fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|text| toml::from_str::<Pricing>(&text).map_err(|err| err.to_string()))
        {
            Ok(pricing) => pricing,
            Err(err) => {
                eprintln!("could not read pricing table {path}: {err}");
                return 1;
            }
        },
        None => embedded_pricing(),
    };
    let input = CostInput {
        ccu,
        room_size,
        intents_per_sec: intents,
        mode,
        state_kb,
        sessions_per_player_day: sessions,
    };
    let estimate = estimate(&input, &pricing);
    if json {
        match serde_json::to_string_pretty(&estimate) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("could not print JSON: {err}");
                return 1;
            }
        }
        return 0;
    }
    print_table(&estimate);
    println!("prices as of {}", pricing.as_of);
    println!("assumptions:");
    for assumption in &estimate.assumptions {
        println!("  - {assumption}");
    }
    0
}

fn usage_error(message: impl std::fmt::Display) -> i32 {
    eprintln!("{message}");
    eprintln!("{USAGE}");
    2
}

/// The next argument, parsed as a number for `flag`.
fn number(args: &[String], i: &mut usize, flag: &str) -> Result<f64, i32> {
    match args.get(*i + 1) {
        Some(value) => match value.parse::<f64>() {
            Ok(parsed) if parsed.is_finite() => {
                *i += 2;
                Ok(parsed)
            }
            _ => Err(usage_error(format!(
                "{flag} expects a number, got {value:?}"
            ))),
        },
        None => Err(usage_error(format!("{flag} expects a number"))),
    }
}

fn integer(args: &[String], i: &mut usize, flag: &str) -> Result<u32, i32> {
    match args.get(*i + 1) {
        Some(value) => match value.parse::<u32>() {
            Ok(parsed) => {
                *i += 2;
                Ok(parsed)
            }
            Err(_) => Err(usage_error(format!(
                "{flag} expects a whole number, got {value:?}"
            ))),
        },
        None => Err(usage_error(format!("{flag} expects a whole number"))),
    }
}

fn positive(value: Option<f64>, flag: &str) -> Result<f64, i32> {
    match value {
        Some(value) if value > 0.0 => Ok(value),
        Some(_) => Err(usage_error(format!("{flag} must be greater than 0"))),
        None => Err(usage_error(format!("{flag} is required"))),
    }
}

fn nonnegative(value: Option<f64>, flag: &str) -> Result<f64, i32> {
    match value {
        Some(value) if value >= 0.0 => Ok(value),
        Some(_) => Err(usage_error(format!("{flag} must be zero or greater"))),
        None => Err(usage_error(format!("{flag} is required"))),
    }
}

fn print_table(estimate: &Estimate) {
    let header = ["ITEM", "QUANTITY", "BILLABLE", "USD"];
    let mut rows: Vec<[String; 4]> = estimate
        .items
        .iter()
        .map(|item| {
            [
                item.name.to_owned(),
                format!("{} {}", number_text(item.quantity), item.unit),
                number_text(item.billable),
                format!("{:.2}", item.usd),
            ]
        })
        .collect();
    rows.push([
        "TOTAL".to_owned(),
        String::new(),
        String::new(),
        format!("{:.2}", estimate.total_usd),
    ]);
    let mut widths = header.map(str::len);
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    print_row(&header.map(str::to_owned), &widths);
    for (index, row) in rows.iter().enumerate() {
        if index + 1 == rows.len() {
            // Last row is the total: separate it from the line items.
            println!(
                "{}",
                "-".repeat(widths.iter().sum::<usize>() + widths.len() - 1)
            );
        }
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

/// Print at most six decimals, without exponents (a $0.000001 line beside a
/// billion-row line).
fn number_text(value: f64) -> String {
    if value == 0.0 {
        "0".to_owned()
    } else if value >= 1000.0 {
        format!("{value:.0}")
    } else if value >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.6}")
    }
}

#[cfg(test)]
mod tests {
    use super::number_text;

    #[test]
    fn number_text_avoids_exponents() {
        assert_eq!(number_text(6_296.479), "6296");
        assert_eq!(number_text(0.25), "0.250000");
        assert_eq!(number_text(0.0), "0");
    }
}
