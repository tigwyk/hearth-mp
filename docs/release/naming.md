# Naming

**Decision (2026-10-26): Hearth MP.** Crates are `hearth-mp` and `hearth-mp-*`,
the Rust path is `hearth_mp`, and the CLI binary is `hearth-mp` (`cargo
hearth-mp`). The frozen protocol v1 strings keep `hearth` (see
[`scripts/wire-constants.txt`](../../scripts/wire-constants.txt)).

**Repository host:** not decided yet. Record it here before the first push.

> Trademark availability is not legal clearance; consult a trademark attorney
> before committing to a name.

## Why not plain "Hearth"

On 2026-10-26, an unrelated project (`wbunting/hearth`, a local-first Rust web
framework) published `hearth`, `hearth-cli` and about 20 other `hearth-*` crates
on crates.io. Another tool owns `hearth-core`, `hearth-api` and `hearth-store`.
The `hearth-` prefix is crowded, and the two names we needed most are gone.

## Availability, checked 2026-10-26

| Where | Name | Result |
| --- | --- | --- |
| crates.io | `hearth`, `hearth-cli`, `hearth-core` | taken (unrelated projects) |
| crates.io | `hearth-mp`, `hearthmp`, `hearth-multiplayer` | free |
| crates.io | `hearth-mp-protocol`, `-sync`, `-client`, `-server`, `-gateway`, `-cli`, `-godot`, `-unity-core`, … (every workspace crate) | free (no `hearth-mp-*` crate exists) |
| npm | `hearth` | taken |
| npm | `hearth-mp`, `hearthmp` | free |
| OpenUPM | `com.hearth.client`, `com.hearth.mp` | free |
| Godot Asset Library | search "hearth" | no results |
| GitHub user/org | `hearth` | taken |
| GitHub user/org | `hearthmp`, `hearth-mp`, `hearth-multiplayer` | free |
| Domains | `hearthmp.dev`, `hearthmp.io`, `hearth-mp.com` | no DNS records (likely unregistered; confirm with a registrar) |
| Domains | `hearthmp.com` | taken (a restaurant in Moorpark, CA) |
| Domains | `hearth.dev` | taken |

No game-networking library or service named Hearth or Hearth MP turned up in web,
GitHub, Unity or Godot searches. Nearby names: the Hearthlink Valheim mod,
`echoo19/hearth` (an AI 2D engine, now a different product), the games Haven &
Hearth and Hearth and Hamlet.

## Risk to check before release

Game-dev search results for "hearth" are dominated by Blizzard's
**Hearthstone**. That hurts discoverability, and a "Hearth" mark in the same
industry is worth a trademark search:
[USPTO](https://tmsearch.uspto.gov/) and [EUIPO](https://euipo.europa.eu/eSearch/).
These searches were not run as part of this check.

## Not renamed

These keep `hearth`, on purpose:

- The frozen protocol v1 strings: `hearth:ping`/`hearth:pong`, the `hearth.*`
  events, the `X-Hearth-*` headers, `/__hearth/*`, the `hearth_*` SQL tables and
  `hearth_schema` (`scripts/wire-constants.txt`, enforced by
  `crates/hearth-mp-protocol/tests/wire_constants.rs`).
- The C ABI of the Unity plugin: the `hearth_unity` library and its `hearth_*`
  exports (`api/hearth_unity.h`).
- The `"hearth"` key in the server's structured logs and the `HEARTH_*`
  environment variables.
- The engine side: the C# namespace `Hearth`, the `Hearth*` classes, the
  `com.hearth.client` Unity package, the Godot `addons/hearth` folder and its
  `Hearth*` class names. None of them collide with anything; renaming them is a
  separate decision.
