use hearth_mp::room;

trait Room {}

struct Game;

#[room(mode = "ticked", max_clients = 2, binding = "GAME")]
impl Room for Game {}

fn main() {}