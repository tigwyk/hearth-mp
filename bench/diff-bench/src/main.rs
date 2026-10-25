//! Hearth v0.4 Task 2: how much does the per-pass diff cost, before and after
//! the public/owner view split?
//!
//! For `clients ∈ {4, 16, 64}` and a per-client view of roughly `{1, 10, 100} KB`,
//! with 10% of all JSON leaves changed per pass, this times both designs:
//!
//! (a) **current**: build every client's full view (public + that client's owner
//!     fields), then `diff` the full view per client.
//! (b) **split** (Task 2): build the public view once, diff it once, then build
//!     and diff only each client's owner view and prepend the shared public ops.
//!
//! View construction is inside the timed region because the runtime serialises a
//! fresh JSON view per client per pass; producing the mutated "after" state is
//! outside it. Everything runs on one thread with `std::time::Instant`. The state
//! is a plain JSON tree, not a `HearthState`: `diff` only sees JSON, and the
//! view split only changes what is merged before the call.

use std::hint::black_box;
use std::time::Instant;

use hearth_sync::diff;
use serde_json::{json, Map, Value};

const CLIENTS: [usize; 3] = [4, 16, 64];
/// Requested per-client view size. The exact size depends on the generated
/// fields, so the table prints the measured view size next to the label.
const VIEW_KB: [usize; 3] = [1, 10, 100];
/// Measured passes per configuration, after `WARMUP` unmeasured ones.
const PASSES: usize = 50;
const WARMUP: usize = 5;
/// Owner fields are this fraction of one client's view; the rest is public.
const OWNER_FRACTION: f64 = 0.2;
/// Fraction of *all* JSON leaves changed per pass.
const CHANGED_FRACTION: f64 = 0.1;
/// Rough bytes per `"field": value, ` pair, used to size the tree.
const BYTES_PER_LEAF: usize = 24;

/// A small deterministic PRNG, so two runs compare exactly the same work.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        // The usual 64-bit LCG constants; the high bits are discarded.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit.max(1) as u64) as usize
    }
}

/// One JSON leaf: a number, a short string or a three-number array.
fn leaf(rng: &mut Rng) -> Value {
    match rng.below(10) {
        0..=6 => json!(rng.below(1_000_000) as i64),
        7..=8 => json!(format!("s{:06}", rng.below(1_000_000))),
        _ => json!([rng.below(100) as i64, rng.below(100) as i64, rng.below(100) as i64]),
    }
}

/// Change one leaf in place, keeping its shape.
fn change_leaf(value: &mut Value, rng: &mut Rng) {
    match value {
        Value::Number(number) => {
            let next = number.as_i64().unwrap_or_default() + 1 + rng.below(1000) as i64;
            *number = next.into();
        }
        Value::String(text) => {
            if let Some(last) = text.pop() {
                text.push(if last == 'z' { 'y' } else { 'z' });
            }
        }
        Value::Array(items) => {
            let index = rng.below(items.len());
            if let Some(item) = items.get_mut(index) {
                change_leaf(item, rng);
            }
        }
        _ => *value = leaf(rng),
    }
}

/// The state as JSON leaves, in insertion order so random mutation is O(1).
///
/// `public` is visible to every client; `owners[client]` is that client's
/// `#[sync(owner)]` fields. The paths are the same for every client; the values
/// differ, which is what makes the current per-client diff distinct work.
struct BenchState {
    public: Vec<(String, Value)>,
    owners: Vec<Vec<(String, Value)>>,
}

impl BenchState {
    fn new(view_bytes: usize, clients: usize) -> Self {
        let owner_bytes = (view_bytes as f64 * OWNER_FRACTION) as usize;
        let public_bytes = view_bytes.saturating_sub(owner_bytes);
        let mut state = Self::with_leaf_counts(
            (public_bytes / BYTES_PER_LEAF).max(1),
            (owner_bytes / BYTES_PER_LEAF).max(1),
            clients,
        );
        // One calibration pass: the random leaf mix is not exactly BYTES_PER_LEAF,
        // so scale the counts once to land on the requested view size.
        let measured = serde_json::to_string(&state.view(0))
            .map(|json| json.len())
            .unwrap_or(0);
        if measured > 0 {
            let scale = view_bytes as f64 / measured as f64;
            let public = (state.public.len() as f64 * scale).round() as usize;
            let owner = (state.owners[0].len() as f64 * scale).round() as usize;
            state = Self::with_leaf_counts(public.max(1), owner.max(1), clients);
        }
        state
    }

    fn with_leaf_counts(public_leaves: usize, owner_leaves: usize, clients: usize) -> Self {
        let seed = (public_leaves as u64) << 32 | (owner_leaves as u64) << 16 | clients as u64;
        let mut rng = Rng::new(seed);
        let public = fields(&mut rng, "p", public_leaves);
        let owners = (0..clients)
            .map(|client| fields(&mut rng, &format!("o{client}_"), owner_leaves))
            .collect();
        Self { public, owners }
    }

    fn leaf_count(&self) -> usize {
        self.public.len() + self.owners.iter().map(Vec::len).sum::<usize>()
    }

    /// The current design's per-client view: public fields plus that client's
    /// owner fields, serialised as one JSON object.
    fn view(&self, client: usize) -> Value {
        let mut object = Map::with_capacity(self.public.len() + self.owners[client].len());
        for (name, value) in self.public.iter().chain(&self.owners[client]) {
            object.insert(name.clone(), value.clone());
        }
        Value::Object(object)
    }

    fn public_view(&self) -> Value {
        Value::Object(
            self.public
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        )
    }

    /// One client's owner fields, already filtered: what Task 2 diffs per client.
    fn owner_view(&self, client: usize) -> Value {
        Value::Object(
            self.owners[client]
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        )
    }

    /// Change `CHANGED_FRACTION` of all leaves, public and per-client owner.
    fn mutate(&mut self, rng: &mut Rng) {
        let total = self.leaf_count();
        let changes = ((total as f64 * CHANGED_FRACTION).round() as usize).max(1);
        for _ in 0..changes {
            let mut index = rng.below(total);
            if index < self.public.len() {
                change_leaf(&mut self.public[index].1, rng);
                continue;
            }
            index -= self.public.len();
            for owner in &mut self.owners {
                if index < owner.len() {
                    change_leaf(&mut owner[index].1, rng);
                    break;
                }
                index -= owner.len();
            }
        }
    }

    fn clone_state(&self) -> Self {
        Self {
            public: self.public.clone(),
            owners: self.owners.clone(),
        }
    }
}

fn fields(rng: &mut Rng, prefix: &str, count: usize) -> Vec<(String, Value)> {
    (0..count)
        .map(|index| (format!("{prefix}{index:05}"), leaf(rng)))
        .collect()
}

/// The measured cost of one pass under one design.
struct Pass {
    ns: u128,
    ops: usize,
}

/// (a): full view + full diff per client.
fn pass_current(state: &BenchState, after: &BenchState, clients: usize) -> Pass {
    let start = Instant::now();
    let mut ops = 0;
    for client in 0..clients {
        let before = state.view(client);
        let now = after.view(client);
        let patch = diff(&before, &now);
        ops += patch.len();
        black_box(patch);
    }
    Pass {
        ns: start.elapsed().as_nanos(),
        ops,
    }
}

/// (b): one public diff, then owner-view diffs per client, prepended.
fn pass_split(state: &BenchState, after: &BenchState, clients: usize) -> Pass {
    let start = Instant::now();
    let public_before = state.public_view();
    let public_after = after.public_view();
    let public_ops = diff(&public_before, &public_after);
    let mut ops = 0;
    for client in 0..clients {
        let owner_before = state.owner_view(client);
        let owner_after = after.owner_view(client);
        // The runtime clones the shared public ops per client; count that cost.
        let mut patch = public_ops.clone();
        patch.extend(diff(&owner_before, &owner_after));
        ops += patch.len();
        black_box(patch);
    }
    Pass {
        ns: start.elapsed().as_nanos(),
        ops,
    }
}

struct Row {
    clients: usize,
    view_bytes: usize,
    current_ms: f64,
    split_ms: f64,
    ops: usize,
}

fn bench(clients: usize, requested_kb: usize) -> Row {
    let view_bytes = requested_kb * 1024;
    let mut state = BenchState::new(view_bytes, clients);
    let measured_view_bytes = serde_json::to_string(&state.view(0))
        .expect("the benchmark state serialises")
        .len();
    let mut rng = Rng::new(((clients as u64) << 32) | requested_kb as u64);

    let mut current_ns = 0u128;
    let mut split_ns = 0u128;
    let mut ops = 0usize;
    for pass in 0..WARMUP + PASSES {
        let mut after = state.clone_state();
        after.mutate(&mut rng);
        let current = pass_current(&state, &after, clients);
        let split = pass_split(&state, &after, clients);
        if pass >= WARMUP {
            current_ns += current.ns;
            split_ns += split.ns;
            // Both designs send the same client-visible patches; (a)'s count is
            // the baseline.
            ops += current.ops;
        }
        state = after;
    }

    Row {
        clients,
        view_bytes: measured_view_bytes,
        current_ms: current_ns as f64 / PASSES as f64 / 1e6,
        split_ms: split_ns as f64 / PASSES as f64 / 1e6,
        ops: ops / PASSES,
    }
}

fn main() {
    println!("diff-bench — Hearth v0.4 Task 2: cost of one handler pass's diffs");
    println!(
        "  {PASSES} measured passes (+{WARMUP} warmup) per row; {:.0}% of all JSON leaves changed per pass",
        CHANGED_FRACTION * 100.0
    );
    println!(
        "  (a) current: full view + full diff per client   (b) split: one public diff + owner diffs per client"
    );
    println!();
    println!(
        "{:>7}  {:>10}  {:>12}  {:>10}  {:>8}  {:>9}",
        "CLIENTS", "VIEW", "CURRENT(ms)", "SPLIT(ms)", "SPEEDUP", "OPS/PASS"
    );

    let mut wins = 0;
    let mut rows = Vec::new();
    for clients in CLIENTS {
        for requested_kb in VIEW_KB {
            let row = bench(clients, requested_kb);
            if row.split_ms <= row.current_ms {
                wins += 1;
            }
            println!(
                "{:>7}  {:>10}  {:>12.3}  {:>10.3}  {:>7.2}x  {:>9}",
                row.clients,
                format!("{:.1} KB", row.view_bytes as f64 / 1024.0),
                row.current_ms,
                row.split_ms,
                row.current_ms / row.split_ms.max(f64::MIN_POSITIVE),
                row.ops,
            );
            rows.push(row);
        }
    }
    println!();
    println!(
        "split is faster in {wins}/{} rows; measured view bytes are close to the requested {VIEW_KB:?} KB",
        rows.len()
    );
}
