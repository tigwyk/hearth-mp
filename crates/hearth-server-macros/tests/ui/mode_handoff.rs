use hearth::room;

trait Room {}

struct Game;

#[room(mode = "handoff")]
impl Room for Game {}

fn main() {}
