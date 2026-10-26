//! Shared deterministic arena simulation for the v0.4 ticked/prediction sample.
//!
//! The same `step` runs in the room's `on_tick` and in the client predictor
//! (`hearth_mp_client::predict::Logic`), so the two must agree exactly. That is why
//! this module is deterministic in practice: it uses only `+ - * /`, `sqrt` and
//! comparisons on `f32`, with no transcendental functions, no platform
//! intrinsics and no iteration over hash-ordered collections.
//!
//! Everyone moves at the same speed (there is only one input kind), so the
//! interesting part is contact: two avatars that end a tick closer than their
//! combined radius are pushed apart. That push-back is what makes the server
//! disagree with a lightly-latency-compensated client, which is the sample's
//! whole point.

use std::collections::BTreeMap;
use std::time::Duration;

use hearth_mp_client::Logic;
use hearth_mp_protocol::PlayerId;
use serde::{Deserialize, Serialize};

/// The arena is the square `[0, ARENA_MAX]²`, which is also the avatar centre's
/// allowed range.
pub const ARENA_MAX: f32 = 20.0;
/// Units per second, in any direction (diagonals are normalised).
pub const SPEED_PER_SECOND: f32 = 4.0;
/// Half the side of the axis-aligned avatar box used for contact.
pub const AVATAR_RADIUS: f32 = 0.5;

/// One avatar as it appears in the synced view.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Avatar {
    pub pos: [f32; 2],
}

/// The synced simulation state. The keys are player ids, which is what makes it
/// deserialisable straight from the client's view.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ArenaState {
    pub avatars: BTreeMap<String, Avatar>,
}

/// `Move` is the only intent: a direction with each component in `-1..=1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    pub dir: [i8; 2],
}

impl Move {
    /// The direction the server actually uses: each component clamped to
    /// `-1..=1`, so `[5, 0]` behaves exactly like `[1, 0]`.
    pub fn clamped(&self) -> [f32; 2] {
        [
            f32::from(self.dir[0].clamp(-1, 1)),
            f32::from(self.dir[1].clamp(-1, 1)),
        ]
    }
}

/// Advance the simulation one fixed tick.
///
/// `inputs` are the moves applied *this* tick, in the order the players asked;
/// a move from a player who is not in the state is ignored. The order of
/// `inputs` cannot change the result: movement is commutative and contact
/// resolution walks the sorted keys.
pub fn step(state: &mut ArenaState, inputs: &[(PlayerId, Move)], dt: f32) {
    let distance = SPEED_PER_SECOND * dt;
    for (player, movement) in inputs {
        let Some(avatar) = state.avatars.get_mut(&player.0) else {
            continue;
        };
        let dir = normalised(movement.clamped());
        avatar.pos[0] = clamp(avatar.pos[0] + dir[0] * distance);
        avatar.pos[1] = clamp(avatar.pos[1] + dir[1] * distance);
    }
    separate_contacts(state);
}

fn normalised(dir: [f32; 2]) -> [f32; 2] {
    let length = (dir[0] * dir[0] + dir[1] * dir[1]).sqrt();
    if length <= f32::EPSILON {
        return [0.0, 0.0];
    }
    [dir[0] / length, dir[1] / length]
}

fn clamp(value: f32) -> f32 {
    value.clamp(0.0, ARENA_MAX)
}

/// Push overlapping avatars apart, walking the sorted keys so the result does not
/// depend on map order or on the order moves arrived in.
fn separate_contacts(state: &mut ArenaState) {
    let keys: Vec<String> = state.avatars.keys().cloned().collect();
    for (index, first) in keys.iter().enumerate() {
        for second in keys.iter().skip(index + 1) {
            let Some((left, right)) = state.avatars.get(first).zip(state.avatars.get(second))
            else {
                continue;
            };
            let dx = right.pos[0] - left.pos[0];
            let dy = right.pos[1] - left.pos[1];
            let distance = (dx * dx + dy * dy).sqrt();
            let overlap = 2.0 * AVATAR_RADIUS - distance;
            if overlap <= 0.0 {
                continue;
            }
            // Distance zero has no direction: separate along +x, deterministically.
            let (nx, ny) = if distance <= f32::EPSILON {
                (1.0, 0.0)
            } else {
                (dx / distance, dy / distance)
            };
            let half = overlap / 2.0;
            let left_pos = [
                clamp(left.pos[0] - nx * half),
                clamp(left.pos[1] - ny * half),
            ];
            let right_pos = [
                clamp(right.pos[0] + nx * half),
                clamp(right.pos[1] + ny * half),
            ];
            if let Some(avatar) = state.avatars.get_mut(first) {
                avatar.pos = left_pos;
            }
            if let Some(avatar) = state.avatars.get_mut(second) {
                avatar.pos = right_pos;
            }
        }
    }
}

/// The shared simulation as a client `Logic` implementation (v0.4 Task 8).
///
/// The server room's `on_tick` and `hearth-mp-client`'s predictor both call
/// [`step`], so they cannot drift apart.
pub struct Arena;

impl Logic for Arena {
    type State = ArenaState;
    type Input = Move;

    fn step(state: &mut ArenaState, inputs: &[(PlayerId, Move)], dt: Duration) {
        crate::step(state, inputs, dt.as_secs_f32());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(name: &str) -> PlayerId {
        PlayerId(name.to_owned())
    }

    fn arena(players: &[(&str, [f32; 2])]) -> ArenaState {
        let mut state = ArenaState::default();
        for (name, pos) in players {
            state
                .avatars
                .insert((*name).to_owned(), Avatar { pos: *pos });
        }
        state
    }

    fn pos(state: &ArenaState, name: &str) -> [f32; 2] {
        state.avatars[name].pos
    }

    const DT: f32 = 0.05;

    #[test]
    fn moves_at_four_units_per_second() {
        let mut state = arena(&[("a", [5.0, 5.0])]);
        step(&mut state, &[(pid("a"), Move { dir: [1, 0] })], DT);
        assert!((pos(&state, "a")[0] - 5.2).abs() < 1e-4);
        assert!((pos(&state, "a")[1] - 5.0).abs() < 1e-4);
    }

    #[test]
    fn diagonal_is_normalised() {
        let mut state = arena(&[("a", [5.0, 5.0])]);
        step(&mut state, &[(pid("a"), Move { dir: [1, 1] })], DT);
        let moved = pos(&state, "a");
        let travelled = ((moved[0] - 5.0).powi(2) + (moved[1] - 5.0).powi(2)).sqrt();
        assert!((travelled - 0.2).abs() < 1e-4, "travelled {travelled}");
    }

    #[test]
    fn out_of_range_components_are_clamped() {
        let mut far = arena(&[("a", [5.0, 5.0])]);
        let mut clamped = arena(&[("a", [5.0, 5.0])]);
        step(&mut far, &[(pid("a"), Move { dir: [5, 0] })], DT);
        step(&mut clamped, &[(pid("a"), Move { dir: [1, 0] })], DT);
        assert_eq!(pos(&far, "a"), pos(&clamped, "a"));
    }

    #[test]
    fn stays_inside_the_arena() {
        let mut state = arena(&[("a", [0.1, 19.9])]);
        for _ in 0..10 {
            step(&mut state, &[(pid("a"), Move { dir: [-1, 1] })], DT);
        }
        assert_eq!(pos(&state, "a"), [0.0, ARENA_MAX]);
    }

    #[test]
    fn overlapping_avatars_are_pushed_apart() {
        let mut state = arena(&[("a", [5.0, 5.0]), ("b", [5.2, 5.0])]);
        step(&mut state, &[], DT);
        let (left, right) = (pos(&state, "a"), pos(&state, "b"));
        let distance = ((right[0] - left[0]).powi(2) + (right[1] - left[1]).powi(2)).sqrt();
        assert!(
            distance >= 2.0 * AVATAR_RADIUS - 1e-4,
            "distance {distance}"
        );
        assert!(left[0] < right[0], "sorted order decides who moves left");
    }

    #[test]
    fn same_inputs_give_the_same_state() {
        let mut first = arena(&[("a", [5.0, 5.0]), ("b", [5.1, 5.0])]);
        let mut second = first.clone();
        let inputs = [
            (pid("a"), Move { dir: [1, 0] }),
            (pid("b"), Move { dir: [0, 1] }),
        ];
        step(&mut first, &inputs, DT);
        step(&mut second, &inputs, DT);
        assert_eq!(first, second);
    }

    #[test]
    fn input_order_does_not_matter() {
        let mut first = arena(&[("a", [5.0, 5.0]), ("b", [5.2, 5.0])]);
        let mut second = first.clone();
        step(
            &mut first,
            &[
                (pid("a"), Move { dir: [1, 0] }),
                (pid("b"), Move { dir: [-1, 0] }),
            ],
            DT,
        );
        step(
            &mut second,
            &[
                (pid("b"), Move { dir: [-1, 0] }),
                (pid("a"), Move { dir: [1, 0] }),
            ],
            DT,
        );
        assert_eq!(first, second);
    }

    #[test]
    fn unknown_players_are_ignored() {
        let mut state = arena(&[("a", [5.0, 5.0])]);
        let before = state.clone();
        step(&mut state, &[(pid("ghost"), Move { dir: [1, 0] })], DT);
        assert_eq!(state, before);
    }
}
