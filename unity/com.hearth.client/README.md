# Hearth Client for Unity

Unity 2022.3+ client for [Hearth](../../README.md) rooms on Cloudflare Durable
Objects: `await` a join, typed state with `OnChange` events, reconnect, snapshot
interpolation, and client-side prediction with your shared Rust logic. Desktop,
mobile and WebGL.

## Install

1. Build the native plugin: `scripts/build-unity-native.sh` (all platforms this
   host can build; `--only webgl` for the WebGL core).
2. Add the package by git URL (`?path=unity/com.hearth.client`) or from disk.
3. Generate typed state from your room crate:
   `hearth codegen --unity Assets/Hearth --manifest-path my-game/Cargo.toml`.

## Use

```csharp
await client.LoginAnonymousAsync();
var name = await client.CreateRoomAsync("tictactoe");
var room = await client.JoinAsync<TicTacToeBoard>("tictactoe", name);
room.State.Cells.OnChange += RenderBoard;
room.Send(TicTacToeIntents.Place(4));
```

Every callback runs on the main thread. `Window/Hearth` shows each client's live
state and patches, and can start `hearth dev` when you press Play.

Ticked rooms: `room.Interpolator(new object[] { "avatars", "*", "pos" })` samples
other players smoothly; `new HearthPredictor<TState>(room, "arena", 20)` predicts
your own with the game's logic compiled into a custom plugin
(`hearth_unity_core::plugin! { logic: { "arena" => arena_logic::Arena } }`, built
with `build-unity-native.sh --crate .. --out Assets/Plugins/Hearth` and the
`HEARTH_CUSTOM_PLUGIN` scripting define). See
[docs/ticked.md](../../docs/ticked.md#8-prediction-in-unity).

## Samples

- `examples/tictactoe-unity`: event room, typed state, `-hearthSmoke` headless check.
- `examples/arena-unity`: ticked room, predicted self, interpolated others,
  `-hearthSmoke -arena` headless check.
