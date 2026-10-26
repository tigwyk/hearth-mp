//! `hearth-mp cost` model checks against a fixed, hand-computable pricing table.
//!
//! Everything here is offline: `estimate` is a pure function, and the two CLI
//! tests run the built binary with no environment.

use std::process::Command;

use hearth_mp_cli::cost::{
    embedded_pricing, estimate, quantities, CostInput, CostMode, DurableObjects, Model, Pricing,
    Workers,
};

/// Round numbers so every expected value in this file is easy to check by hand.
fn test_pricing() -> Pricing {
    Pricing {
        as_of: "2026-01-01".to_owned(),
        sources: vec![
            "https://example.test/workers".to_owned(),
            "https://example.test/durable-objects".to_owned(),
        ],
        workers: Workers {
            base_usd_month: 5.0,
            requests_included: 1_000_000.0,
            usd_per_million_requests: 1.0,
        },
        durable_objects: DurableObjects {
            requests_included: 1_000_000.0,
            usd_per_million_requests: 10.0,
            gb_s_included: 1_000_000.0,
            usd_per_million_gb_s: 1.0,
            memory_gb: 0.128,
            ws_incoming_ratio: 20.0,
            rows_written_included: 1_000_000_000.0,
            usd_per_million_rows_written: 1.0,
            storage_gb_included: 1.0,
            usd_per_gb_month: 1.0,
        },
        model: Model {
            seconds_per_month: 1_000_000.0,
            idle_hibernate_after_s: 10.0,
        },
    }
}

fn event_input(ccu: f64, room_size: f64, intents_per_sec: f64) -> CostInput {
    CostInput {
        ccu,
        room_size,
        intents_per_sec,
        mode: CostMode::Event,
        state_kb: 4.0,
        sessions_per_player_day: 2.0,
    }
}

fn ticked_input(ccu: f64, room_size: f64, intents: f64, hz: f64, persist_every: u32) -> CostInput {
    CostInput {
        mode: CostMode::Ticked {
            hz,
            persist_every_ticks: persist_every,
        },
        ..event_input(ccu, room_size, intents)
    }
}

#[test]
fn idle_free_tier_is_base_only() {
    // ccu 10, 1 intent/s: every dimension stays under its included allowance.
    let estimate = estimate(&event_input(10.0, 4.0, 1.0), &test_pricing());
    assert_eq!(estimate.total_usd, 5.0);
    for item in &estimate.items {
        assert_eq!(item.billable, 0.0, "{}", item.name);
        assert_eq!(item.usd, 0.0, "{}", item.name);
    }
}

#[test]
fn event_mode_hand_computed() {
    let pricing = test_pricing();
    let input = event_input(1000.0, 4.0, 2.0);
    let q = quantities(&input, &pricing);
    // rooms 250, msgs 2, S 1e6.
    assert_eq!(q.ws_msgs, 2_000_000_000.0);
    // 2e9 / 20 + 60,000 connects.
    assert_eq!(q.do_requests, 100_060_000.0);
    // 250 rooms * 1e6 s * 0.128 GB.
    assert_eq!(q.gb_s, 32_000_000.0);
    // 250 rooms * 8 frames/s * 1e6 s.
    assert_eq!(q.rows_written, 2_000_000_000.0);

    let estimate = estimate(&input, &pricing);
    // 5 base + 99,060,000 DO requests * $10/million
    //         + 31,000,000 GB-s * $1/million
    //         + 1,000,000,000 rows * $1/million.
    assert!(
        (estimate.total_usd - 2026.6).abs() < 0.01,
        "{}",
        estimate.total_usd
    );
}

#[test]
fn batching_caps_messages_at_20() {
    let pricing = test_pricing();
    let batched = quantities(&event_input(1000.0, 4.0, 20.0), &pricing);
    let over = quantities(&event_input(1000.0, 4.0, 50.0), &pricing);
    assert_eq!(over.msgs_per_player, 20.0);
    assert_eq!(over.ws_msgs, batched.ws_msgs);
    assert_eq!(
        estimate(&event_input(1000.0, 4.0, 50.0), &pricing).total_usd,
        estimate(&event_input(1000.0, 4.0, 20.0), &pricing).total_usd
    );
}

#[test]
fn ticked_bills_full_duration_and_watchdog_alarms() {
    let pricing = test_pricing();
    let input = ticked_input(1000.0, 4.0, 2.0, 10.0, 20);
    let q = quantities(&input, &pricing);
    assert_eq!(q.active_fraction, 1.0);
    // 250 rooms * 10 Hz * 1e6 s. The ticked runtime drives these from an
    // in-object loop, so they are duration, not Durable Object requests.
    assert_eq!(q.ticks, 2_500_000_000.0);
    // 250 rooms * 1e6 s / 300 s: one watchdog alarm per room per five minutes.
    assert_eq!(q.watchdog_alarms, 250.0 * 1_000_000.0 / 300.0);
    // ws 2e9 / 20 + 60,000 connects + the watchdog alarms; ticks add none.
    let expected = 100_000_000.0 + 60_000.0 + q.watchdog_alarms;
    assert!((q.do_requests - expected).abs() < 1e-6);
    let estimate = estimate(&input, &pricing);
    let requests = estimate
        .items
        .iter()
        .find(|item| item.name == "durable objects requests")
        .unwrap();
    assert_eq!(requests.quantity, q.do_requests);
}

#[test]
fn persist_every_reduces_rows() {
    let pricing = test_pricing();
    let every_20 = quantities(&ticked_input(1000.0, 4.0, 2.0, 10.0, 20), &pricing);
    let every_10 = quantities(&ticked_input(1000.0, 4.0, 2.0, 10.0, 10), &pricing);
    assert_eq!(every_20.rows_written, 125_000_000.0);
    assert_eq!(every_10.rows_written, 250_000_000.0);
    assert!(every_20.rows_written < every_10.rows_written);
}

#[test]
fn zero_intents_event_rooms_hibernate() {
    let q = quantities(&event_input(1000.0, 4.0, 0.0), &test_pricing());
    assert_eq!(q.msgs_per_player, 0.0);
    assert_eq!(q.room_msg_rate, 0.0);
    assert_eq!(q.active_fraction, 0.0);
    assert_eq!(q.gb_s, 0.0);
}

#[test]
fn embedded_pricing_parses_and_has_as_of() {
    let pricing = embedded_pricing();
    assert_eq!(pricing.as_of.len(), 10, "as_of must be YYYY-MM-DD");
    assert!(pricing.as_of.contains('-'), "{}", pricing.as_of);
    assert!(pricing.sources.len() >= 2);
    assert!(pricing.workers.base_usd_month > 0.0);
    assert!(pricing.durable_objects.usd_per_million_requests > 0.0);
    assert_eq!(pricing.durable_objects.ws_incoming_ratio, 20.0);
    let estimate = estimate(&event_input(100.0, 4.0, 1.0), &pricing);
    assert!(estimate.total_usd.is_finite() && estimate.total_usd > 0.0);
}

#[test]
fn cli_rejects_nonpositive_ccu_exit_2() {
    let output = Command::new(env!("CARGO_BIN_EXE_hearth-mp"))
        .args(["cost", "--ccu", "0", "--room-size", "4", "--intents", "2"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--ccu"), "{stderr}");
}

#[test]
fn json_output_round_trips() {
    let output = Command::new(env!("CARGO_BIN_EXE_hearth-mp"))
        .args([
            "cost",
            "--ccu",
            "1000",
            "--room-size",
            "4",
            "--intents",
            "2",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    let pricing = embedded_pricing();
    let expected = estimate(&event_input(1000.0, 4.0, 2.0), &pricing);
    let total = printed["total_usd"].as_f64().unwrap();
    assert!((total - expected.total_usd).abs() < 1e-9);
    assert_eq!(
        printed["items"].as_array().unwrap().len(),
        expected.items.len()
    );
    assert!(!printed["assumptions"].as_array().unwrap().is_empty());
    // A hand spot check: 262,860,000 DO requests - 1,000,000 included.
    let requests = printed["items"][1].clone();
    assert_eq!(requests["name"], "durable objects requests");
    assert!((requests["quantity"].as_f64().unwrap() - 262_860_000.0).abs() < 1e-6);
}
