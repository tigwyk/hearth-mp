use hearth_mp::room;

trait Room {}

struct Game;

#[room(mode = "event")]
impl Room for Game {}

fn main() {}
