use hearth_sync::HearthState;
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(HearthState, Serialize, Deserialize, Default)]
struct Match {
    #[sync(owner)]
    hand: u32,
}

fn main() {}
