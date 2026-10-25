use std::rc::Rc;
use std::time::Duration;

use hearth_protocol::{PlayerId, Value};
use serde::Serialize;

use crate::host::Sql;
use crate::room::{Client, Room, TimerKey};

/// A side effect buffered by [`Ctx`]. The runtime applies effects only when the
/// handler returns `Ok`; a failed handler drops them.
pub(crate) enum Effect {
    Event {
        to: Option<PlayerId>,
        name: String,
        data: Value,
    },
    Timer {
        key: TimerKey,
        after: Duration,
    },
    CancelTimer {
        key: TimerKey,
    },
    Kick(PlayerId),
    Lock,
    Dispose,
}

/// The room's capabilities for one handler. State changes are diffed and
/// flushed when the handler returns; sends, timers, kick, lock and dispose are
/// buffered and applied only on success.
pub struct Ctx<R: Room> {
    state: R::State,
    clients: Vec<Client>,
    effects: Vec<Effect>,
    sql: Rc<dyn Sql>,
    now_ms: u64,
}

impl<R: Room> Ctx<R> {
    pub(crate) fn new(
        state: R::State,
        clients: Vec<Client>,
        sql: Rc<dyn Sql>,
        now_ms: u64,
    ) -> Self {
        Self {
            state,
            clients,
            effects: Vec::new(),
            sql,
            now_ms,
        }
    }

    /// Mutable room state. Persisted and diffed when the handler returns `Ok`.
    pub fn state(&mut self) -> &mut R::State {
        &mut self.state
    }

    /// The roster at the start of this handler.
    pub fn clients(&self) -> &[Client] {
        &self.clients
    }

    /// Send one event to one player.
    pub fn send(&mut self, who: &PlayerId, name: &str, data: impl Serialize) {
        self.effects.push(Effect::Event {
            to: Some(who.clone()),
            name: name.to_owned(),
            data: to_json(data),
        });
    }

    /// Send one event to every joined player.
    pub fn broadcast(&mut self, name: &str, data: impl Serialize) {
        self.effects.push(Effect::Event {
            to: None,
            name: name.to_owned(),
            data: to_json(data),
        });
    }

    /// Schedule a durable timer. A second timer with the same key replaces the
    /// first.
    pub fn timer(&mut self, key: impl Into<TimerKey>, after: Duration) {
        self.effects.push(Effect::Timer {
            key: key.into(),
            after,
        });
    }

    pub fn cancel_timer(&mut self, key: impl Into<TimerKey>) {
        self.effects.push(Effect::CancelTimer { key: key.into() });
    }

    /// The room's SQLite database.
    pub fn sql(&self) -> &dyn Sql {
        self.sql.as_ref()
    }

    pub fn kick(&mut self, who: &PlayerId) {
        self.effects.push(Effect::Kick(who.clone()));
    }

    /// Refuse new joins until the room empties or is unlocked by a later plan.
    pub fn lock(&mut self) {
        self.effects.push(Effect::Lock);
    }

    /// Wipe storage and release the room after this handler.
    pub fn dispose(&mut self) {
        self.effects.push(Effect::Dispose);
    }

    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub(crate) fn into_parts(self) -> (R::State, Vec<Effect>) {
        (self.state, self.effects)
    }
}

fn to_json(value: impl Serialize) -> Value {
    serde_json::to_value(value).expect("Hearth event data must be JSON-serialisable")
}
