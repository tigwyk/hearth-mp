use hearth::room;

trait Room {}

struct Game;

#[room(mode = "ticked")]
impl Room for Game {}

fn main() {}
