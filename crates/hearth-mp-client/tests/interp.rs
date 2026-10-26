//! Clock-sync and snapshot-interpolation conformance.
//!
//! The fixture is shared with the Godot (v0.4 Task 6) and Unity (Task 7) ports:
//! every case feeds `snapshots` through `Interpolator::on_patch` in order and
//! samples a tracked path at `render_t`. `track`/`sample` are optional path
//! fields (default `["value"]`); `track` may contain `"*"` wildcards.

use hearth_mp_client::{ClockSync, Interpolator};
use serde_json::{json, Value};

/// The path every case tracks by default; the snapshot value is stored under it.
const PATH: &str = "value";

fn fixture_cases() -> Vec<Value> {
    hearth_mp_protocol::vectors::load("interp.json")
}

/// An optional path field of a fixture case, defaulting to `["value"]`.
fn case_path(case: &Value, field: &str) -> Vec<Value> {
    match case.get(field) {
        Some(path) => path
            .as_array()
            .unwrap_or_else(|| panic!("`{field}` must be a path array"))
            .clone(),
        None => vec![json!(PATH)],
    }
}

/// Numeric comparison: JSON cannot tell `5` from `5.0`, and lerped values are
/// floats even when the inputs were integers.
fn values_match(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Number(actual), Value::Number(expected)) => {
            match (actual.as_f64(), expected.as_f64()) {
                (Some(actual), Some(expected)) => {
                    (actual - expected).abs() <= 1e-9 * (1.0 + expected.abs())
                }
                _ => false,
            }
        }
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| values_match(actual, expected))
        }
        _ => actual == expected,
    }
}

#[test]
fn fixture_cases_match() {
    let cases = fixture_cases();
    assert!(!cases.is_empty(), "the fixture has cases");

    for case in cases {
        let name = case["name"].as_str().expect("every case is named");
        let tick_hz =
            u8::try_from(case["tick_hz"].as_u64().expect("tick_hz")).expect("tick_hz fits u8");
        let tracked_path = case_path(&case, "track");
        let sample_path = match case.get("sample") {
            Some(_) => case_path(&case, "sample"),
            None => tracked_path.clone(),
        };

        let mut interpolator = Interpolator::new(tick_hz);
        interpolator.track(tracked_path);

        for pair in case["snapshots"].as_array().expect("snapshots is an array") {
            let server_t = pair[0].as_u64().expect("snapshot time");
            let state = json!({ PATH: pair[1].clone() });
            interpolator.on_patch(&state, server_t);
        }

        let render_t = case["render_t"].as_u64().expect("render_t");
        let actual = interpolator.sample(&sample_path, render_t);
        let expected = &case["expect"];
        if expected.is_null() {
            assert!(
                actual.is_none(),
                "case `{name}`: expected None, got {actual:?}"
            );
        } else {
            let actual =
                actual.unwrap_or_else(|| panic!("case `{name}`: expected {expected}, got None"));
            assert!(
                values_match(&actual, expected),
                "case `{name}`: expected {expected}, got {actual}"
            );
        }
    }
}

#[test]
fn wildcard_tracks_every_key() {
    let mut interpolator = Interpolator::new(10);
    let pattern = vec![json!(PATH), json!("*"), json!("pos")];
    interpolator.track(pattern);

    interpolator.on_patch(&json!({ PATH: {"a": {"pos": 0}, "b": {"pos": 100}} }), 1000);
    interpolator.on_patch(
        &json!({ PATH: {"a": {"pos": 10}, "b": {"pos": 200}} }),
        1100,
    );

    let a = vec![json!(PATH), json!("a"), json!("pos")];
    let b = vec![json!(PATH), json!("b"), json!("pos")];
    assert_eq!(interpolator.sample(&a, 1050), Some(json!(5.0)));
    assert_eq!(interpolator.sample(&b, 1050), Some(json!(150.0)));

    // A key that only exists in the newest snapshot has no bracket to lerp
    // from, and a path covered by no pattern is not tracked at all.
    let c = vec![json!(PATH), json!("c"), json!("pos")];
    assert_eq!(interpolator.sample(&c, 1050), None);
    assert_eq!(interpolator.sample(&[json!("elsewhere")], 1050), None);
}

#[test]
fn untracked_or_missing_path_returns_none() {
    let mut interpolator = Interpolator::new(10);
    let path = vec![json!(PATH)];
    interpolator.track(path.clone());

    // A patch that lacks the tracked path records "missing", not a value.
    interpolator.on_patch(&json!({"other": 1}), 1000);
    assert_eq!(interpolator.sample(&path, 1000), None);
    assert_eq!(interpolator.sample(&[json!("untracked")], 1000), None);

    // An empty interpolator has no snapshots at all.
    assert_eq!(Interpolator::new(10).sample(&path, 0), None);
}

#[test]
fn clock_offset_min_delay_wins() {
    let mut clock = ClockSync::new();
    assert_eq!(clock.server_now(0), None);

    // A delayed arrival implies a smaller offset than a quicker one.
    clock.observe(10_000, 1_200); // +8800
    clock.observe(11_000, 1_050); // +9950 (less delay)
    assert_eq!(clock.server_now(1_000), Some(10_950));

    // A later but more delayed arrival must not lower the estimate.
    clock.observe(12_000, 2_300); // +9700
    assert_eq!(clock.server_now(1_000), Some(10_950));
}

#[test]
fn clock_jump_resets() {
    let mut clock = ClockSync::new();
    for i in 0..8 {
        clock.observe(50_000 + i * 100, 1_000 + i * 100);
    }
    assert_eq!(clock.server_now(0), Some(49_000));

    // The server clock jumps forward by 5 s: old samples are discarded.
    clock.observe(55_100, 1_900); // +53_200
    assert_eq!(clock.server_now(0), Some(53_200));
    assert_eq!(clock.server_now(500), Some(53_700));
}

#[test]
fn render_time_is_two_periods_behind() {
    let mut clock = ClockSync::new();
    clock.observe(10_000, 1_000); // server = local + 9000

    // 10 Hz: 100 ms period, so render 200 ms behind the estimated server now.
    assert_eq!(
        Interpolator::new(10).render_time(&clock, 5_000),
        Some(13_800)
    );
    // 20 Hz: 50 ms period, so render 100 ms behind.
    assert_eq!(
        Interpolator::new(20).render_time(&clock, 5_000),
        Some(13_900)
    );

    // Without a clock estimate there is no render time.
    assert_eq!(
        Interpolator::new(10).render_time(&ClockSync::new(), 5_000),
        None
    );
}
