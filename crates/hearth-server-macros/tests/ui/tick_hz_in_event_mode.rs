use hearth::room;

trait Room {}

struct Game;

#[room(mode = "event", max_clients = 2, binding = "GAME", tick_hz = 20)]
impl Room for Game {}

fn main() {}