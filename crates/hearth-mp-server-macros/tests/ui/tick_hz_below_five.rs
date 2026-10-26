use hearth_mp::room;

trait Room {}

struct Game;

#[room(mode = "ticked", max_clients = 2, binding = "GAME", tick_hz = 2)]
impl Room for Game {}

fn main() {}