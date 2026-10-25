use hearth::room;

trait Room {}

struct Game;

#[room(mode = "ticked", max_clients = 2, binding = "GAME", tick_hz = 30)]
impl Room for Game {}

fn main() {}