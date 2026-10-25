use hearth_sync::Schema;
use serde::Serialize;

#[allow(dead_code)]
#[derive(Schema, Serialize)]
#[serde(tag = "kind")]
enum Tagged {
    A { x: u8 },
}

fn main() {}
