//! Hearth v0.4 Task 1: alarm-driven vs in-isolate-loop tick driver benchmark.
//!
//! Two Durable Objects tick at the same fixed rate with the same CPU load; the
//! only difference is the driver:
//!
//! - [`AlarmTicker`] schedules the next tick with the Alarms API: an absolute
//!   schedule (`start + n * period`) that survives eviction and does not drift.
//! - [`LoopTicker`] holds the object open with `state.wait_until` and loops on
//!   `worker::Delay` for the remaining time to the next slot.
//!
//! Both accept one hibernatable WebSocket and send one byte per tick, so the
//! client can measure receive jitter as well as the server-side interval error.
//! `run.mjs` drives every combination in parallel and writes `results.json`.
//!
//! This Worker is standalone on purpose: it depends on `worker` only, so the
//! measurement is the platform, not the Hearth runtime.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use worker::{
    Date, DateInit, Env, Method, Request, Response, Result, ScheduledTime, State, WebSocketPair,
};

/// The durable "a run was started" marker, so a fresh instance can tell that its
/// in-memory state was evicted mid-run.
const RUN_KEY: &str = "tick-bench-run";
const MIN_HZ: u32 = 1;
const MAX_HZ: u32 = 100;
const MAX_SECS: u64 = 3600;
const MAX_WORK_US: u64 = 100_000;
const DEFAULT_HZ: u32 = 20;
const DEFAULT_SECS: u64 = 60;

// `#[worker::durable_object]`'s wasm-bindgen expansion spells the crate as bare
// `wasm_bindgen` (the same workaround as the Hearth room macro).
#[allow(unused_imports)]
use worker::wasm_bindgen as wasm_bindgen;

fn now_ms() -> f64 {
    // V8's Date.now has millisecond resolution; see the README.
    worker::js_sys::Date::now()
}

fn scheduled_time(slot_ms: f64) -> ScheduledTime {
    let at: worker::js_sys::Date =
        Date::from(DateInit::Millis(slot_ms.max(0.0).round() as u64)).into();
    ScheduledTime::new(at)
}

/// Burn `work_us` microseconds of CPU, the way a game tick would.
fn burn(work_us: u64) {
    let until = now_ms() + work_us as f64 / 1000.0;
    while now_ms() < until {
        std::hint::black_box(());
    }
}

/// One byte per connected client per tick; the client measures arrival times.
fn send_tick(state: &State) {
    for socket in state.get_websockets() {
        let _ = socket.send_with_bytes(vec![b'.']);
    }
}

#[derive(Debug, Deserialize)]
struct StartQuery {
    hz: Option<u32>,
    secs: Option<u64>,
    work_us: Option<u64>,
}

struct StartParams {
    hz: u32,
    secs: u64,
    work_us: u64,
}

impl StartParams {
    fn from_request(req: &Request) -> std::result::Result<Self, String> {
        let query: StartQuery = req
            .query()
            .map_err(|_| "invalid query string".to_owned())?;
        let params = Self {
            hz: query.hz.unwrap_or(DEFAULT_HZ),
            secs: query.secs.unwrap_or(DEFAULT_SECS),
            work_us: query.work_us.unwrap_or(0),
        };
        if !(MIN_HZ..=MAX_HZ).contains(&params.hz) {
            return Err(format!("hz must be {MIN_HZ}..={MAX_HZ}"));
        }
        if !(1..=MAX_SECS).contains(&params.secs) {
            return Err(format!("secs must be 1..={MAX_SECS}"));
        }
        if params.work_us > MAX_WORK_US {
            return Err(format!("work_us must be 0..={MAX_WORK_US}"));
        }
        Ok(params)
    }

    fn period_ms(&self) -> f64 {
        1000.0 / f64::from(self.hz)
    }

    fn expected(&self) -> u64 {
        self.secs.saturating_mul(u64::from(self.hz))
    }
}

/// One run's in-memory state. The loop driver shares it across the detached tick
/// task and the event handlers, so it lives in an `Rc<RefCell<..>>`; no borrow is
/// ever held across an `await`.
struct Run {
    period_ms: f64,
    work_us: u64,
    start_ms: f64,
    end_ms: f64,
    expected: u64,
    /// The next tick slot to run (1-based): slot `n` is due at
    /// `start_ms + n * period_ms`.
    next_slot: u64,
    ticks: u64,
    /// Slots skipped because a wake was late, plus the unrun tail at the end.
    /// Invariant once stopped: `ticks + missed == expected`.
    missed: u64,
    /// `|actual - scheduled|` for every executed tick, in milliseconds.
    errors_ms: Vec<f64>,
    stopped: bool,
    /// Set by every `/start`; a stale loop driver exits when it no longer matches.
    generation: u64,
}

impl Run {
    fn new(params: &StartParams, generation: u64) -> Self {
        let start_ms = now_ms();
        Self {
            period_ms: params.period_ms(),
            work_us: params.work_us,
            start_ms,
            end_ms: start_ms + params.secs as f64 * 1000.0,
            expected: params.expected(),
            next_slot: 1,
            ticks: 0,
            missed: 0,
            errors_ms: Vec::with_capacity(params.expected() as usize),
            stopped: false,
            generation,
        }
    }

    fn next_slot_ms(&self) -> f64 {
        self.start_ms + self.next_slot as f64 * self.period_ms
    }

    /// One wake: run the slot that contains `now`. A late wake skips the slots it
    /// missed (counted as `missed`) instead of running them back to back, and the
    /// absolute schedule means the error does not accumulate.
    fn advance(&mut self, now: f64, state: &State) {
        let current = ((now - self.start_ms) / self.period_ms)
            .floor()
            .clamp(0.0, self.expected as f64) as u64;
        if current < self.next_slot {
            return; // Woke early; the driver waits for the slot again.
        }
        self.missed += current - self.next_slot;
        let scheduled = self.start_ms + current as f64 * self.period_ms;
        self.errors_ms.push((now - scheduled).max(0.0));
        self.next_slot = current + 1;
        self.ticks += 1;
        burn(self.work_us);
        send_tick(state);
        if self.next_slot > self.expected || now >= self.end_ms {
            // Whatever slots are left at the end were dropped.
            self.missed = self.expected.saturating_sub(self.ticks);
            self.stopped = true;
        }
    }
}

/// The durable copy of a run, used only to detect an eviction: if a request finds
/// the marker but no in-memory `Run`, the instance was evicted or hibernated.
#[derive(Serialize, Deserialize)]
struct RunMarker {
    hz: u32,
    secs: u64,
    work_us: u64,
    expected: u64,
    start_ms: f64,
}

#[derive(Serialize, Default)]
struct Percentiles {
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn percentiles(values: &[f64]) -> Percentiles {
    if values.is_empty() {
        return Percentiles::default();
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = |percent: f64| -> f64 {
        // Nearest-rank: the smallest value at or above the requested percentile.
        let index = ((percent / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[index.saturating_sub(1).min(sorted.len() - 1)]
    };
    Percentiles {
        p50: rank(50.0),
        p95: rank(95.0),
        p99: rank(99.0),
        max: sorted[sorted.len() - 1],
    }
}

#[derive(Serialize)]
struct Stats {
    ticks: u64,
    expected: u64,
    missed: u64,
    interval_err_ms: Percentiles,
    evicted: bool,
}

/// The stats of the in-memory run, if any.
fn memory_stats(run: &RefCell<Option<Run>>) -> Option<Stats> {
    let borrow = run.borrow();
    let run = borrow.as_ref()?;
    Some(Stats {
        ticks: run.ticks,
        expected: run.expected,
        missed: run.missed,
        interval_err_ms: percentiles(&run.errors_ms),
        evicted: false,
    })
}

/// `GET /stats`: the in-memory run, or the durable marker's story of an eviction.
async fn read_stats(state: &State, run: &RefCell<Option<Run>>) -> Result<Stats> {
    if let Some(stats) = memory_stats(run) {
        return Ok(stats);
    }
    let marker: Option<RunMarker> = state.storage().get(RUN_KEY).await?;
    Ok(match marker {
        // The instance was evicted mid-run: the counters are gone with it.
        Some(marker) => Stats {
            ticks: 0,
            expected: marker.expected,
            missed: marker.expected,
            interval_err_ms: Percentiles::default(),
            evicted: true,
        },
        None => Stats {
            ticks: 0,
            expected: 0,
            missed: 0,
            interval_err_ms: Percentiles::default(),
            evicted: false,
        },
    })
}

/// Reset the run and write the durable marker before the driver starts ticking.
async fn begin(
    state: &State,
    run: &RefCell<Option<Run>>,
    params: &StartParams,
    generation: u64,
) -> Result<()> {
    let new_run = Run::new(params, generation);
    state
        .storage()
        .put(
            RUN_KEY,
            RunMarker {
                hz: params.hz,
                secs: params.secs,
                work_us: params.work_us,
                expected: new_run.expected,
                start_ms: new_run.start_ms,
            },
        )
        .await?;
    *run.borrow_mut() = Some(new_run);
    Ok(())
}

/// The hibernatable WebSocket both classes accept: one per client, never woken by
/// the client, only used as a tick delivery channel.
fn upgrade(state: &State, req: &Request) -> Result<Response> {
    let is_websocket = req
        .headers()
        .get("Upgrade")?
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    if !is_websocket {
        return Response::error("expected a websocket upgrade", 426);
    }
    let pair = WebSocketPair::new()?;
    state.accept_web_socket(&pair.server);
    Response::from_websocket(pair.client)
}

/// Alarm-driven ticker: each alarm runs one slot, then schedules the next slot on
/// the absolute schedule.
#[worker::durable_object]
pub struct AlarmTicker {
    state: Rc<State>,
    run: RefCell<Option<Run>>,
    generation: Cell<u64>,
}

impl AlarmTicker {
    async fn start(&self, req: &Request) -> Result<Response> {
        let params = match StartParams::from_request(req) {
            Ok(params) => params,
            Err(message) => return Response::error(message, 400),
        };
        // A previous run may have left an alarm pending.
        self.state.storage().delete_alarm().await?;
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        begin(&self.state, &self.run, &params, generation).await?;
        let next = self
            .run
            .borrow()
            .as_ref()
            .map(Run::next_slot_ms)
            .expect("begin just stored a run");
        self.state
            .storage()
            .set_alarm(scheduled_time(next))
            .await?;
        Response::empty().map(|response| response.with_status(204))
    }
}

impl worker::DurableObject for AlarmTicker {
    fn new(state: State, _env: Env) -> Self {
        Self {
            state: Rc::new(state),
            run: RefCell::new(None),
            generation: Cell::new(0),
        }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Get, "/ws") => upgrade(&self.state, &req),
            (Method::Post, "/start") => self.start(&req).await,
            (Method::Get, "/stats") => {
                let stats = read_stats(&self.state, &self.run).await?;
                Response::from_json(&stats)
            }
            _ => Response::error("not found", 404),
        }
    }

    async fn alarm(&self) -> Result<Response> {
        // An alarm with no in-memory run means the instance was evicted
        // (hibernated) mid-run; `/stats` reports that from the durable marker.
        if self.run.borrow().is_none() {
            self.state.storage().delete_alarm().await?;
            return Response::empty();
        }
        let now = now_ms();
        let next = {
            let mut borrow = self.run.borrow_mut();
            let run = borrow.as_mut().expect("checked above");
            run.advance(now, &self.state);
            (!run.stopped).then(|| run.next_slot_ms())
        };
        match next {
            Some(next) => {
                self.state
                    .storage()
                    .set_alarm(scheduled_time(next))
                    .await?
            }
            None => self.state.storage().delete_alarm().await?,
        }
        Response::empty()
    }

    // The client only receives; never panic on a frame or a close.
    async fn websocket_message(
        &self,
        _socket: worker::WebSocket,
        _message: worker::WebSocketIncomingMessage,
    ) -> Result<()> {
        Ok(())
    }

    async fn websocket_close(
        &self,
        _socket: worker::WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        Ok(())
    }

    async fn websocket_error(
        &self,
        _socket: worker::WebSocket,
        _error: worker::Error,
    ) -> Result<()> {
        Ok(())
    }
}

/// In-isolate loop ticker: the first `/start` launches one loop that waits out
/// each slot with `worker::Delay`, and the loop exits when the run stops or a
/// newer `/start` supersedes it.
#[worker::durable_object]
pub struct LoopTicker {
    state: Rc<State>,
    run: Rc<RefCell<Option<Run>>>,
    generation: Cell<u64>,
}

impl LoopTicker {
    async fn start(&self, req: &Request) -> Result<Response> {
        let params = match StartParams::from_request(req) {
            Ok(params) => params,
            Err(message) => return Response::error(message, 400),
        };
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        begin(&self.state, &self.run, &params, generation).await?;
        let state = Rc::clone(&self.state);
        let run = Rc::clone(&self.run);
        // A detached `spawn_local` future is cancelled when this request
        // finishes, so the loop runs inside `wait_until`: the Workers API that
        // keeps the object alive while the loop is outstanding.
        self.state
            .wait_until(async move { loop_ticks(state, run, generation).await });
        Response::empty().map(|response| response.with_status(204))
    }
}

impl worker::DurableObject for LoopTicker {
    fn new(state: State, _env: Env) -> Self {
        Self {
            state: Rc::new(state),
            run: Rc::new(RefCell::new(None)),
            generation: Cell::new(0),
        }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Get, "/ws") => upgrade(&self.state, &req),
            (Method::Post, "/start") => self.start(&req).await,
            (Method::Get, "/stats") => {
                let stats = read_stats(&self.state, &self.run).await?;
                Response::from_json(&stats)
            }
            _ => Response::error("not found", 404),
        }
    }

    // The loop driver does not use alarms.
    async fn alarm(&self) -> Result<Response> {
        Response::empty()
    }

    async fn websocket_message(
        &self,
        _socket: worker::WebSocket,
        _message: worker::WebSocketIncomingMessage,
    ) -> Result<()> {
        Ok(())
    }

    async fn websocket_close(
        &self,
        _socket: worker::WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        Ok(())
    }

    async fn websocket_error(
        &self,
        _socket: worker::WebSocket,
        _error: worker::Error,
    ) -> Result<()> {
        Ok(())
    }
}

/// Wait out the next slot, run it, repeat until stopped or superseded.
async fn loop_ticks(state: Rc<State>, run: Rc<RefCell<Option<Run>>>, generation: u64) {
    loop {
        let wait_ms = {
            let borrow = run.borrow();
            let Some(current) = borrow.as_ref() else {
                return;
            };
            if current.generation != generation || current.stopped {
                return;
            }
            current.next_slot_ms() - now_ms()
        };
        if wait_ms > 0.0 {
            // `Delay` never resolves early; rounding up keeps the tick on time.
            worker::Delay::from(Duration::from_millis(wait_ms.ceil() as u64)).await;
        }
        let mut borrow = run.borrow_mut();
        let Some(current) = borrow.as_mut() else {
            return;
        };
        if current.generation != generation || current.stopped {
            return;
        }
        current.advance(now_ms(), &state);
        let stopped = current.stopped;
        drop(borrow);
        if stopped {
            return;
        }
    }
}

/// The Worker entry: `/<alarm|loop>/<object-name>/<ws|start|stats>` routes to the
/// named Durable Object of the matching class. The DO URL is built here, so the
/// client never talks to a DO directly.
#[worker::event(fetch)]
async fn main(req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    let path = req.path();
    if path.is_empty() || path == "/" {
        return Response::ok(
            "tick-bench: POST /<alarm|loop>/<name>/start?hz=&secs=&work_us=, \
             GET /<alarm|loop>/<name>/ws (websocket), GET /<alarm|loop>/<name>/stats",
        );
    }
    let Some((driver, rest)) = path.trim_start_matches('/').split_once('/') else {
        return Response::error("not found", 404);
    };
    let Some((name, do_path)) = rest.split_once('/') else {
        return Response::error("not found", 404);
    };
    let binding = match driver {
        "alarm" => "ALARM_TICKER",
        "loop" => "LOOP_TICKER",
        _ => return Response::error("not found", 404),
    };
    let namespace = env.durable_object(binding)?;
    let stub = namespace.get_by_name(name)?;

    let mut url = worker::Url::parse("https://do")?;
    url.set_path(&format!("/{do_path}"));
    url.set_query(req.url()?.query());
    let headers = worker::Headers::new();
    for (key, value) in req.headers().entries() {
        // Forward the WebSocket handshake, but never a framing header for a body
        // this router does not forward.
        if matches!(
            key.to_ascii_lowercase().as_str(),
            "host" | "content-length" | "transfer-encoding"
        ) {
            continue;
        }
        headers.set(&key, &value)?;
    }
    let mut init = worker::RequestInit::new();
    init.method = req.method();
    init.headers = headers;
    let outgoing = Request::new_with_init(url.as_str(), &init)?;
    stub.fetch_with_request(outgoing).await
}
