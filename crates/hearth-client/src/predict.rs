//! Client-side prediction for ticked rooms.
//!
//! The game's shared logic crate implements [`Logic`]; [`Predictor`] keeps a
//! locally stepped copy of the state and reconciles it with the server on every
//! `ACK`: it rebuilds from the authoritative state and replays the inputs the
//! server has not applied yet.
//!
//! The trait adds `Serialize + Default` to the plan's `State` bounds: the Godot
//! wrapper renders the predicted state as a Dictionary, and a fresh predictor
//! needs a state before the first snapshot arrives.

use std::collections::VecDeque;
use std::time::Duration;

use hearth_protocol::{PlayerId, Value};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// The game's deterministic simulation, shared by the server room and the
/// predicting client.
///
/// `State` mirrors the synced view so a prediction can be rebuilt from an
/// authoritative snapshot. Implementations must be deterministic: the same
/// state and inputs must give the same result on the server and the client.
pub trait Logic {
    /// The synced state shape, deserialisable from the client's view and
    /// serialisable for rendering.
    type State: DeserializeOwned + Serialize + Clone + Default;
    /// One player input.
    type Input: Serialize + Clone;

    /// Advance the simulation one fixed tick with `inputs` applied this tick.
    fn step(state: &mut Self::State, inputs: &[(PlayerId, Self::Input)], dt: Duration);
}

/// Records unacknowledged inputs and replays them over the authoritative state.
pub struct Predictor<L: Logic> {
    me: PlayerId,
    dt: Duration,
    predicted: L::State,
    /// Inputs sent but not yet acknowledged, in send order.
    unacked: VecDeque<(u32, L::Input)>,
}

impl<L: Logic> Predictor<L> {
    /// A predictor for `me` in a room ticking at `tick_hz`.
    pub fn new(me: PlayerId, tick_hz: u8) -> Self {
        Self {
            me,
            dt: Duration::from_secs_f64(1.0 / f64::from(tick_hz.max(1))),
            predicted: L::State::default(),
            unacked: VecDeque::new(),
        }
    }

    /// Record input `seq` and step the prediction one tick immediately.
    pub fn input(&mut self, seq: u32, input: L::Input) {
        self.unacked.push_back((seq, input.clone()));
        L::step(&mut self.predicted, &[(self.me.clone(), input)], self.dt);
    }

    /// Reconcile with the authoritative state after an `ACK`.
    ///
    /// Drops inputs covered by `acked` (every seq up to its maximum), rebuilds
    /// the prediction from `server_state`, and replays the remaining inputs in
    /// order, one step each. An unreadable server state leaves the previous
    /// prediction in place and keeps the inputs for the next ACK.
    pub fn on_authoritative(&mut self, server_state: &Value, acked: &[u32]) {
        if let Some(max_acked) = acked.iter().copied().max() {
            self.unacked.retain(|(seq, _)| *seq > max_acked);
        }
        let Ok(mut state) = serde_json::from_value::<L::State>(server_state.clone()) else {
            return;
        };
        for (_, input) in &self.unacked {
            L::step(&mut state, &[(self.me.clone(), input.clone())], self.dt);
        }
        self.predicted = state;
    }

    /// Drop inputs the server reported as unconfirmed (never applied).
    pub fn on_unconfirmed(&mut self, seqs: &[u32]) {
        self.unacked.retain(|(seq, _)| !seqs.contains(seq));
    }

    /// The locally predicted state.
    pub fn predicted(&self) -> &L::State {
        &self.predicted
    }
}
