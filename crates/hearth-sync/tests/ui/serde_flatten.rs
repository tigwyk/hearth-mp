use hearth_sync::Schema;
use serde::Serialize;

#[allow(dead_code)]
#[derive(Schema, Serialize)]
struct Flattened {
    #[serde(flatten)]
    inner: Inner,
}

#[allow(dead_code)]
#[derive(Serialize)]
struct Inner {
    x: u8,
}

fn main() {}
