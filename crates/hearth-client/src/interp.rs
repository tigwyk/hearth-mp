//! Clock synchronisation and snapshot interpolation for ticked rooms.
//!
//! A ticked room stamps every `PATCH` with its tick and its millisecond clock
//! (`t`). The client renders slightly in the past (two tick periods) and
//! interpolates between the snapshots it has seen, so motion stays smooth even
//! though the server updates at 5-20 Hz.
//!
//! Tracked paths may contain a `*` segment (for example
//! `["avatars", "*", "pos"]`), which matches every key or list index at that
//! level: one snapshot records each concrete path.
//!
//! Both helpers are sans-IO: the caller feeds observations and snapshots and
//! asks for a value at a local or server time.

use std::collections::VecDeque;

use serde_json::{Number, Value};

/// Number of clock samples kept; the least-delayed one wins.
const CLOCK_SAMPLES: usize = 32;

/// A newest sample further than this from the estimate is a server clock jump.
const CLOCK_JUMP_MS: i128 = 1_000;

/// How long snapshots are kept, in server milliseconds.
const SNAPSHOT_WINDOW_MS: u64 = 1_000;

/// A tracked path segment that matches every key/index at that level.
const WILDCARD: &str = "*";

/// Estimates the offset between the server clock and the local clock.
///
/// The offset implied by one arrival is `server_t - local_recv_ms`: network
/// delay only makes it smaller, so the largest of the recent samples (the
/// least-delayed arrival) is the best estimate.
#[derive(Clone, Debug, Default)]
pub struct ClockSync {
    /// Implied offsets in arrival order; at most [`CLOCK_SAMPLES`].
    offsets: Vec<i128>,
}

impl ClockSync {
    /// A clock with no observations yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the server timestamp `server_t` received at local time
    /// `local_recv_ms`.
    pub fn observe(&mut self, server_t: u64, local_recv_ms: u64) {
        let offset = i128::from(server_t) - i128::from(local_recv_ms);
        if let Some(current) = self.offsets.iter().max() {
            // A jump of more than a second makes the old window meaningless.
            if (offset - current).abs() > CLOCK_JUMP_MS {
                self.offsets.clear();
            }
        }
        self.offsets.push(offset);
        if self.offsets.len() > CLOCK_SAMPLES {
            self.offsets.remove(0);
        }
    }

    /// The estimated server time at local time `local_ms`, or `None` before the
    /// first observation (or when the estimate would be negative).
    pub fn server_now(&self, local_ms: u64) -> Option<u64> {
        let offset = self.offsets.iter().max()?;
        u64::try_from(i128::from(local_ms) + offset).ok()
    }
}

/// One state snapshot, reduced to the tracked paths' concrete values.
struct Snapshot {
    server_t: u64,
    /// Every concrete path matched by a tracked pattern, and its value.
    values: Vec<(Vec<Value>, Value)>,
}

/// Interpolates tracked state values between the snapshots of a ticked room.
pub struct Interpolator {
    /// Tick period in milliseconds (`1000 / tick_hz`).
    period_ms: u64,
    /// The tracked path patterns; `*` segments match every key at their level.
    paths: Vec<Vec<Value>>,
    /// Ordered by `server_t`.
    snapshots: VecDeque<Snapshot>,
}

impl Interpolator {
    /// An interpolator for a room ticking at `tick_hz`.
    pub fn new(tick_hz: u8) -> Self {
        Self {
            period_ms: 1_000 / u64::from(tick_hz.max(1)),
            paths: Vec::new(),
            snapshots: VecDeque::new(),
        }
    }

    /// Track `path` in every snapshot from now on. A `"*"` segment matches
    /// every key (or list index) at that level. Tracking a path twice is a
    /// no-op.
    pub fn track(&mut self, path: Vec<Value>) {
        if !self.paths.contains(&path) {
            self.paths.push(path);
        }
    }

    /// Record a snapshot of every tracked path from `state`.
    ///
    /// A snapshot that is not newer than the newest kept one is discarded: two
    /// snapshots for the same instant would make the bracket ambiguous.
    pub fn on_patch(&mut self, state: &Value, server_t: u64) {
        if let Some(newest) = self.snapshots.back() {
            if server_t <= newest.server_t {
                return;
            }
        }
        let mut values = Vec::new();
        for pattern in &self.paths {
            collect(state, pattern, &mut Vec::new(), &mut values);
        }
        self.snapshots.push_back(Snapshot { server_t, values });

        // Keep a one-second window, but never fewer than the two snapshots a
        // bracket needs: a stalled stream can leave a wider gap than a second.
        while self.snapshots.len() > 2 {
            let oldest = match self.snapshots.front() {
                Some(snapshot) => snapshot.server_t,
                None => break,
            };
            if server_t.saturating_sub(oldest) <= SNAPSHOT_WINDOW_MS {
                break;
            }
            self.snapshots.pop_front();
        }
    }

    /// Forget every snapshot, keeping the tracked paths. Called when a
    /// `FULL_STATE` replaces the state, so old snapshots cannot leak into the
    /// new state's interpolation.
    pub fn clear(&mut self) {
        self.snapshots.clear();
    }

    /// The server time to render at: the estimated server now minus two tick
    /// periods, so there is always a newer snapshot to interpolate towards.
    pub fn render_time(&self, clock: &ClockSync, local_ms: u64) -> Option<u64> {
        clock
            .server_now(local_ms)
            .map(|server_now| server_now.saturating_sub(2 * self.period_ms))
    }

    /// Interpolate `path` at server time `render_t`.
    ///
    /// Numbers and equal-length arrays of numbers are lerped; any other value
    /// takes the older snapshot's. Past the newest snapshot the value is
    /// extrapolated for at most one tick period and then held. Before the
    /// oldest snapshot, and with a single snapshot, the available value is
    /// returned. The result is `None` when no tracked pattern matches `path`,
    /// when there are no snapshots, or when either side of the bracket is
    /// missing it.
    pub fn sample(&self, path: &[Value], render_t: u64) -> Option<Value> {
        if !self
            .paths
            .iter()
            .any(|pattern| pattern_matches(pattern, path))
        {
            return None;
        }
        let oldest = self.snapshots.front()?;
        if render_t < oldest.server_t {
            return value_in(oldest, path).cloned();
        }

        let mut left = oldest;
        for right in self.snapshots.iter().skip(1) {
            if render_t < right.server_t {
                return bracket(left, right, path, render_t);
            }
            left = right;
        }
        extrapolate(&self.snapshots, path, render_t, self.period_ms)
    }
}

/// Collect every concrete path (and value) matched by `pattern` from `state`.
fn collect(
    state: &Value,
    pattern: &[Value],
    prefix: &mut Vec<Value>,
    out: &mut Vec<(Vec<Value>, Value)>,
) {
    let Some((segment, rest)) = pattern.split_first() else {
        out.push((prefix.clone(), state.clone()));
        return;
    };
    if segment.as_str() == Some(WILDCARD) {
        match state {
            Value::Object(object) => {
                for (key, child) in object {
                    prefix.push(Value::String(key.clone()));
                    collect(child, rest, prefix, out);
                    prefix.pop();
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    prefix.push(Value::from(index as u64));
                    collect(child, rest, prefix, out);
                    prefix.pop();
                }
            }
            _ => {}
        }
    } else if let Some(child) = child(state, segment) {
        prefix.push(segment.clone());
        collect(child, rest, prefix, out);
        prefix.pop();
    }
}

/// Follow one concrete path segment into a JSON object or array.
fn child<'a>(state: &'a Value, segment: &Value) -> Option<&'a Value> {
    match state {
        Value::Object(object) => object.get(segment.as_str()?),
        Value::Array(items) => items.get(usize::try_from(segment.as_u64()?).ok()?),
        _ => None,
    }
}

/// Whether a tracked `pattern` (which may contain `*`) covers `path`.
fn pattern_matches(pattern: &[Value], path: &[Value]) -> bool {
    pattern.len() == path.len()
        && pattern
            .iter()
            .zip(path)
            .all(|(pattern, path)| pattern.as_str() == Some(WILDCARD) || pattern == path)
}

/// The value recorded for the concrete `path` in `snapshot`, if present.
fn value_in<'a>(snapshot: &'a Snapshot, path: &[Value]) -> Option<&'a Value> {
    snapshot
        .values
        .iter()
        .find_map(|(concrete, value)| (concrete.as_slice() == path).then_some(value))
}

/// Sample the bracket `left..right` at `render_t` (which lies in `left`'s half
/// of the interval).
fn bracket(left: &Snapshot, right: &Snapshot, path: &[Value], render_t: u64) -> Option<Value> {
    let from = value_in(left, path)?;
    let to = value_in(right, path)?;
    let span = right.server_t.saturating_sub(left.server_t);
    if span == 0 {
        return Some(to.clone());
    }
    let ratio = render_t.saturating_sub(left.server_t) as f64 / span as f64;
    Some(lerp(from, to, ratio).unwrap_or_else(|| from.clone()))
}

/// Sample past (or exactly at) the newest snapshot: linear for at most one
/// period, then hold the value reached at that cap.
fn extrapolate(
    snapshots: &VecDeque<Snapshot>,
    path: &[Value],
    render_t: u64,
    period_ms: u64,
) -> Option<Value> {
    let newest = snapshots.back()?;
    let newest_value = value_in(newest, path)?.clone();
    if snapshots.len() < 2 {
        return Some(newest_value);
    }
    let previous = snapshots.iter().rev().nth(1)?;
    let previous_value = value_in(previous, path)?.clone();
    let target = render_t.min(newest.server_t.saturating_add(period_ms));
    let span = newest.server_t.saturating_sub(previous.server_t);
    if span == 0 {
        return Some(newest_value);
    }
    let ratio = target.saturating_sub(previous.server_t) as f64 / span as f64;
    Some(lerp(&previous_value, &newest_value, ratio).unwrap_or(newest_value))
}

/// Linear interpolation for numbers and equal-length number arrays. Anything
/// else returns `None`, so the caller can fall back to a step.
fn lerp(from: &Value, to: &Value, ratio: f64) -> Option<Value> {
    if ratio == 0.0 {
        return Some(from.clone());
    }
    if ratio == 1.0 {
        return Some(to.clone());
    }
    match (from, to) {
        (Value::Number(from), Value::Number(to)) => {
            let from = from.as_f64()?;
            let to = to.as_f64()?;
            Number::from_f64(from + (to - from) * ratio).map(Value::Number)
        }
        (Value::Array(from), Value::Array(to)) if from.len() == to.len() => {
            let mut out = Vec::with_capacity(from.len());
            for (from, to) in from.iter().zip(to) {
                let from = from.as_f64()?;
                let to = to.as_f64()?;
                out.push(Value::Number(Number::from_f64(from + (to - from) * ratio)?));
            }
            Some(Value::Array(out))
        }
        _ => None,
    }
}
