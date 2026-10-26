# Hearth schema document, version 1

Status: **frozen** (Hearth 1.0). Normative, with RFC 2119 key words. A
`SchemaDoc` describes one room's synced state and its intents, so a code
generator in any language can build typed client code. `hearth-mp codegen` writes
one per room; the Godot and Unity generators consume exactly this format.

## 1. Document

```json
{
  "hearth_schema": 1,
  "room": "TicTacToe",
  "hash": "2f028abb7253298e",
  "state": "Board",
  "intent": "Move",
  "types": { "<Rust type name>": TypeDef, ... }
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `hearth_schema` | number | the format version; `1` |
| `room` | string | the room's name (used to prefix generated class names) |
| `hash` | string | the room's schema hash, 16 lowercase hex characters |
| `state` | string | the key in `types` of the room's state struct |
| `intent` | string | the key in `types` of the room's intent enum |
| `types` | object | every named type reachable from `state` and `intent`, by Rust type name |

A consumer MUST ignore fields it does not know.

**The hash is opaque.** Only equality matters: the client sends it in `HELLO`
and the room compares it with its own (`spec/protocol-v1.md` section 5). A
generator MUST copy it, never compute it.

## 2. `TypeDef`

Tagged by `kind`:

```json
{ "kind": "struct", "fields": [ Field, ... ] }
{ "kind": "enum", "variants": [ Variant, ... ] }
```

`Field` is `{ "name": string, "ty": TypeRef, "vis": Vis }`. `Variant` is
`{ "name": string, "shape": Shape }`. Order is significant only as the
declaration order (useful for stable output).

For the **state** struct only synced fields appear; server-only fields never
reach clients and are not in the document.

## 3. `TypeRef`

A string for scalars, otherwise a one-key object:

| `TypeRef` | Wire value |
| --- | --- |
| `"bool"` | boolean |
| `"i8"`, `"i16"`, `"i32"`, `"i64"`, `"u8"`, `"u16"`, `"u32"`, `"u64"` | integer (JSON number) |
| `"f32"`, `"f64"` | number |
| `"string"` | string |
| `"player_id"` | string |
| `"unit"` | `null` |
| `{"option": T}` | `null` or a `T` value |
| `{"list": T}` | array of `T` |
| `{"array": [T, n]}` | array of exactly `n` `T` values |
| `{"map": [K, V]}` | object; keys are the `K` values as strings |
| `{"sync_map": [K, V]}` | object, as `map` (the server diffs it per key) |
| `{"tuple": [T1, T2, ...]}` | array with one value per element type |
| `{"named": "Name"}` | the value of `types["Name"]` |

`i64`/`u64` values above 2^53 lose precision in JSON and GDScript clients;
schemas SHOULD use strings for identifiers.

## 4. `Shape` and `Vis`

`Shape` describes an enum variant's payload:

| `Shape` | Meaning |
| --- | --- |
| `"unit"` | no payload |
| `{"newtype": T}` | one value |
| `{"tuple": [T1, ...]}` | positional values |
| `{"struct": [Field, ...]}` | named fields |

`Vis` is `"public"` (every client sees the field) or `"owner"` (each client sees
only its own entry: the field is a map keyed by player id, filtered to the
viewer).

## 5. Wire form of structs and enums

- A **struct** is a JSON object with one entry per field, by `name`.
- An **enum** value uses serde's *external tagging*:

  | Shape | Wire value |
  | --- | --- |
  | `"unit"` | the variant name as a string: `"Rematch"` |
  | `{"newtype": T}` | `{"<Variant>": <T value>}`: `{"Won": "anon_a"}` |
  | `{"tuple": [...]}` | `{"<Variant>": [<values>]}` |
  | `{"struct": [...]}` | `{"<Variant>": {<fields>}}`: `{"Place": {"cell": 4}}` |

Intents are sent in this form inside `INTENT.intents[].intent`; state enums
appear in it inside the state tree.

## 6. Worked example

The tic-tac-toe room (`examples/tictactoe`) exports:

```json
{
  "hearth_schema": 1,
  "room": "TicTacToe",
  "hash": "2f028abb7253298e",
  "state": "Board",
  "intent": "Move",
  "types": {
    "Board": {
      "kind": "struct",
      "fields": [
        { "name": "cells", "ty": { "list": "u8" }, "vis": "public" },
        { "name": "marks", "ty": { "sync_map": ["player_id", "u8"] }, "vis": "public" },
        { "name": "turn", "ty": "player_id", "vis": "public" },
        { "name": "status", "ty": { "named": "Status" }, "vis": "public" }
      ]
    },
    "Move": {
      "kind": "enum",
      "variants": [
        { "name": "Place", "shape": { "struct": [ { "name": "cell", "ty": "u8", "vis": "public" } ] } },
        { "name": "Rematch", "shape": "unit" }
      ]
    },
    "Status": {
      "kind": "enum",
      "variants": [
        { "name": "Waiting", "shape": "unit" },
        { "name": "Playing", "shape": "unit" },
        { "name": "Won", "shape": { "newtype": "player_id" } },
        { "name": "Draw", "shape": "unit" }
      ]
    }
  }
}
```

A state view for it:

```json
{ "cells": [1, 0, 0, 0, 2, 0, 0, 0, 0], "marks": { "anon_a": 1, "anon_b": 2 },
  "turn": "anon_a", "status": "Playing" }
```

and the intent to place in cell 4 is `{"Place": {"cell": 4}}`.

## 7. Change policy

Within `hearth_schema: 1` only additive changes are allowed: new optional
document fields and new `TypeRef` or `Shape` forms. A generator MUST fail with a
clear message on a `TypeRef` or `Shape` it does not know, rather than emit wrong
code. Anything else needs `hearth_schema: 2`.
