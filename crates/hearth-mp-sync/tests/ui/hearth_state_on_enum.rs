use hearth_mp_sync::HearthState;

#[allow(dead_code)]
#[derive(HearthState)]
enum Card {
    Num(u8),
    Joker,
}

fn main() {}
