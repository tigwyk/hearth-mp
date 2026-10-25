use hearth::room;

trait Room {}

struct Game;

#[room(mode = "event", max_clients = 2, binding = "GAME", intent_burst = 0)]
impl Room for Game {}

fn main() {}
