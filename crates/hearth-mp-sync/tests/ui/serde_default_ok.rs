//! `#[serde(default)]` does not change the wire shape, so it stays allowed.
use hearth_mp_sync::Schema;
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(Schema, Serialize, Deserialize)]
struct WithDefault {
    #[serde(default)]
    count: u8,
}

#[allow(dead_code)]
#[derive(Schema, Serialize, Deserialize)]
struct ContainerDefault {
    #[serde(default)]
    name: String,
}

fn main() {}
