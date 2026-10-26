//! Plan Task 6 Step 5 (v0.1) and v0.2a Task 1 Step 5: hibernation must be
//! invisible to clients, including across drops and resumes.

mod support;

use std::time::Duration;

use hearth_mp_test::{fuzz_hibernation, Step};
use serde_json::json;
use support::{CounterRoom, GraceZeroRoom};

#[test]
fn hibernation_is_invisible_to_clients() {
    // 3 joins (the third is refused: MAX_CLIENTS is 2, identically in both
    // runs), 30 mixed intents, 2 leaves and advances.
    let mut script = vec![Step::Join("a"), Step::Join("b"), Step::Join("c")];
    for i in 0..30u32 {
        let player = ["a", "b", "c"][(i % 3) as usize];
        let intent = match i % 3 {
            0 => json!({"Add": 1}),
            1 => json!({"Secret": format!("s{i}")}),
            _ => json!({"Timer": [format!("t{i}"), 500]}),
        };
        script.push(Step::Intent(player, intent));
        if i % 10 == 9 {
            script.push(Step::Advance(Duration::from_millis(750)));
        }
    }
    script.push(Step::Leave("a"));
    script.push(Step::Advance(Duration::from_millis(750)));
    script.push(Step::Leave("b"));
    script.push(Step::Advance(Duration::from_secs(61)));

    for seed in 0..200 {
        fuzz_hibernation::<GraceZeroRoom>(seed, &script)
            .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
    }
}

#[test]
fn reconnect_is_invisible_to_clients() {
    // Drops and resumes must survive hibernation untouched. Alternate seeds so
    // a resume sometimes lands inside the grace window and sometimes after it.
    for seed in 0..200 {
        let mut script = vec![Step::Join("a"), Step::Join("b"), Step::Join("c")];
        script.push(Step::Intent("a", json!({"Add": 1})));
        script.push(Step::Intent("b", json!({"Secret": "s"})));
        script.push(Step::Drop("a"));
        script.push(Step::Advance(Duration::from_millis(500)));
        script.push(Step::Resume("a"));
        script.push(Step::Intent("a", json!({"Add": 2})));
        script.push(Step::Drop("b"));
        if seed % 2 == 0 {
            script.push(Step::Advance(Duration::from_secs(5)));
        } else {
            script.push(Step::Advance(Duration::from_secs(40)));
        }
        script.push(Step::Resume("b"));
        script.push(Step::Intent("b", json!({"Add": 3})));
        script.push(Step::Advance(Duration::from_secs(61)));

        fuzz_hibernation::<CounterRoom>(seed, &script)
            .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
    }
}
