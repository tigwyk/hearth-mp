use hearth::service;

trait Service {}

struct Stats;

#[service]
impl Service for Stats {}

fn main() {}