use hearth_mp_sync::HearthState;
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(HearthState, Serialize, Deserialize, Default)]
struct State {
    #[sync]
    #[serde(rename = "other")]
    count: u8,
}

fn main() {}
