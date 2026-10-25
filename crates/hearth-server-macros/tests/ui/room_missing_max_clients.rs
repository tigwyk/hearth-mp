use hearth::room;

trait Room {}

struct Game;

#[room(mode = "event")]
impl Room for Game {}

fn main() {}
