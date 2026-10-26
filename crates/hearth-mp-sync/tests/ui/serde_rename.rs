use hearth_mp_sync::Schema;
use serde::Serialize;

#[allow(dead_code)]
#[derive(Schema, Serialize)]
struct Renamed {
    #[serde(rename = "other")]
    field: u8,
}

fn main() {}
