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

use hearth_mp_protocol::{PlayerId, Value};
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
        if let Ok(state) = serde_json::from_value::<L::State>(server_state.clone()) {
            self.rebuild(state);
        }
    }

    /// Rebuild from an already-decoded authoritative state and replay the
    /// unacked inputs.
    fn rebuild(&mut self, mut state: L::State) {
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

/// An object-safe predictor over JSON, so a native plugin can hold predictors
/// for different [`Logic`] types behind one handle type (the Unity
/// `hearth_predictor_*` exports). Unlike [`Predictor::on_authoritative`], a
/// state or input that does not decode is an `Err`, so the caller can report it.
pub trait DynPredictor {
    /// Record input `seq` (JSON for `L::Input`) and step immediately.
    fn input(&mut self, seq: u32, input: &Value) -> Result<(), String>;
    /// Reconcile with the authoritative state (JSON for `L::State`).
    fn on_authoritative(&mut self, state: &Value, acked: &[u32]) -> Result<(), String>;
    /// Drop inputs the server reported as unconfirmed.
    fn on_unconfirmed(&mut self, seqs: &[u32]);
    /// The predicted state as JSON.
    fn predicted(&self) -> Result<Value, String>;
}

impl<L> DynPredictor for Predictor<L>
where
    L: Logic,
    L::Input: DeserializeOwned,
{
    fn input(&mut self, seq: u32, input: &Value) -> Result<(), String> {
        let input = serde_json::from_value(input.clone()).map_err(|e| format!("bad input: {e}"))?;
        Predictor::input(self, seq, input);
        Ok(())
    }

    fn on_authoritative(&mut self, state: &Value, acked: &[u32]) -> Result<(), String> {
        let state = serde_json::from_value::<L::State>(state.clone())
            .map_err(|e| format!("authoritative state does not match the logic state: {e}"))?;
        if let Some(max_acked) = acked.iter().copied().max() {
            self.unacked.retain(|(seq, _)| *seq > max_acked);
        }
        self.rebuild(state);
        Ok(())
    }

    fn on_unconfirmed(&mut self, seqs: &[u32]) {
        Predictor::on_unconfirmed(self, seqs);
    }

    fn predicted(&self) -> Result<Value, String> {
        serde_json::to_value(&self.predicted).map_err(|e| e.to_string())
    }
}

/// A boxed [`DynPredictor`] for `L`.
pub fn boxed<L>(me: PlayerId, tick_hz: u8) -> Box<dyn DynPredictor>
where
    L: Logic + 'static,
    L::Input: DeserializeOwned,
{
    Box::new(Predictor::<L>::new(me, tick_hz))
}

/// The built-in 1-D logic (`"hearth.echo1d"`) the Unity package tests and docs
/// use: the state is `{"x": n}` (other fields are ignored) and each input, clamped
/// to `-1..=1`, is added to `x`.
pub struct Echo1d;

/// [`Echo1d`]'s state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct Echo1dState {
    /// The counter.
    pub x: i64,
}

impl Logic for Echo1d {
    type State = Echo1dState;
    type Input = i64;

    fn step(state: &mut Echo1dState, inputs: &[(PlayerId, i64)], _dt: Duration) {
        for (_, input) in inputs {
            state.x += (*input).clamp(-1, 1);
        }
    }
}
