use hearth_mp::room;

trait Room {}

struct Game;

#[room(mode = "event", max_clients = 0)]
impl Room for Game {}

fn main() {}
