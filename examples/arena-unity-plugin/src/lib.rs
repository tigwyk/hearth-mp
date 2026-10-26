//! The arena's Unity plugin: every Hearth export plus the `"arena"` logic, so
//! `new HearthPredictor<ArenaRoomArena>(room, "arena", 20)` predicts with the
//! exact `step` the server runs.

hearth_unity_core::plugin! { logic: { "arena" => arena_logic::Arena } }
