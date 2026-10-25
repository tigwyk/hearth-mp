use hearth::room;

trait Room {}

struct Game;

#[room(mode = "event", max_clients = 2)]
impl Room for Game {}

fn main() {}