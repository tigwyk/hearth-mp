//! Socket-less Hearth services.
//!
//! A [`Service`] has the same durable state, timers and SQL as a room, but no
//! sockets: it is reached only through typed object-to-object calls
//! ([`crate::Callable`]). Player data, a matchmaker, a leaderboard — anything
//! that outlives or spans matches — is a service.

use std::rc::Rc;
use std::time::Duration;

use hearth_protocol::Value;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::call::{
    rpc_from_value, rpc_to_value, CallError, Callable, Caller, Envelope, HostCaller, Target,
};
use crate::ctx::Effect;
use crate::host::{Host, Sql};
use crate::room::TimerKey;
use crate::store;
use crate::Result;

/// One socket-less Hearth object.
///
/// `self` is rebuilt from [`Default`] after every hibernation, so keep all
/// durable data in `State`, not in the struct's fields.
#[allow(async_fn_in_trait)]
pub trait Service: Default + 'static {
    type State: Serialize + DeserializeOwned + Default;
    type Rpc: Serialize + DeserializeOwned;
    type Reply: Serialize + DeserializeOwned;

    /// Handle one call from another object.
    async fn on_call(
        &mut self,
        ctx: &mut ServiceCtx<Self>,
        from: &Target,
        rpc: Self::Rpc,
    ) -> Result<Self::Reply>;

    /// A durable timer fired.
    async fn on_timer(&mut self, _ctx: &mut ServiceCtx<Self>, _key: TimerKey) -> Result<()> {
        Ok(())
    }
}

/// A service handler's capabilities: state, SQL, timers and object-to-object
/// calls. State changes and timer effects are applied only when the handler
/// returns `Ok`; a failed handler rolls the state back and drops its effects.
pub struct ServiceCtx<S: Service> {
    state: S::State,
    effects: Vec<Effect>,
    sql: Rc<dyn Sql>,
    now_ms: u64,
    caller: Caller,
}

impl<S: Service> ServiceCtx<S> {
    pub(crate) fn new(state: S::State, sql: Rc<dyn Sql>, now_ms: u64, caller: Caller) -> Self {
        Self {
            state,
            effects: Vec::new(),
            sql,
            now_ms,
            caller,
        }
    }

    /// Mutable service state. Persisted when the handler returns `Ok`.
    pub fn state(&mut self) -> &mut S::State {
        &mut self.state
    }

    /// The object's SQLite database.
    pub fn sql(&self) -> &dyn Sql {
        self.sql.as_ref()
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

    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    /// This object's own address.
    pub fn me(&self) -> &Target {
        self.caller.me()
    }

    /// Call another object and wait for its reply. The call is sent at once and
    /// is **not** rolled back if this handler later fails.
    pub async fn call<T: Callable>(
        &mut self,
        name: &str,
        rpc: T::Rpc,
    ) -> std::result::Result<T::Reply, CallError> {
        self.caller.call::<T>(name, rpc).await
    }

    /// Send a durable call that fires after this handler commits. It is
    /// retried until the target answers, and deduplicated by the receiver.
    pub fn notify<T: Callable>(&mut self, name: &str, rpc: T::Rpc) {
        self.effects.push(Effect::Notify {
            binding: T::BINDING.to_owned(),
            name: name.to_owned(),
            body: rpc_to_value(rpc).unwrap_or(Value::Null),
        });
    }

    pub(crate) fn into_parts(self) -> (S::State, Vec<Effect>) {
        (self.state, self.effects)
    }
}

/// The durable object behind a [`Service`]: state in `hearth_state`, timers in
/// `hearth_timers`, and the single alarm.
pub struct ServiceRuntime<S: Service, H: Host> {
    host: Rc<H>,
    binding: &'static str,
    service: S,
    state: Option<S::State>,
    last_saved: Option<Vec<u8>>,
    name: Option<String>,
    incompatible: bool,
}

impl<S: Service, H: Host + 'static> ServiceRuntime<S, H> {
    pub fn new(host: H, binding: &'static str) -> Self {
        Self {
            host: Rc::new(host),
            binding,
            service: S::default(),
            state: None,
            last_saved: None,
            name: None,
            incompatible: false,
        }
    }

    pub fn state(&self) -> Option<&S::State> {
        self.state.as_ref()
    }

    /// The object's own address, using `name` for a service that has not
    /// learned its name yet (a call always carries it in a header).
    pub fn target(&self, name: &str) -> Target {
        let name = self.name.as_deref().unwrap_or(name);
        Target::new(self.binding, name)
    }

    /// Load the persisted state, creating the fixed tables on first use.
    pub async fn load(&mut self) -> Result<()> {
        if self.ensure_loaded().await? {
            anyhow::bail!("persisted state was written by an incompatible build");
        }
        Ok(())
    }

    /// Handle one call. The reply is JSON-shaped and travels back as
    /// `Result<Value, CallError>`.
    pub async fn call(
        &mut self,
        envelope: Envelope,
        name: &str,
    ) -> std::result::Result<Value, CallError> {
        let result = self.handle_call(envelope, name).await;
        if let Err(CallError::Handler(message)) = &result {
            self.log_json(serde_json::json!({
                "hearth": "handler_error",
                "object": self.label(),
                "msg": message,
            }));
        }
        result
    }

    async fn handle_call(
        &mut self,
        envelope: Envelope,
        name: &str,
    ) -> std::result::Result<Value, CallError> {
        if self.ensure_loaded().await.map_err(internal)? {
            return Err(CallError::Handler(
                "persisted state was written by an incompatible build".to_owned(),
            ));
        }
        if self.name.is_none() {
            self.remember_name(name)?;
        }
        let me = self.target(name);
        if envelope.chain.contains(&me) {
            return Err(CallError::ReentrantCall(me));
        }
        let rpc = rpc_from_value::<S::Rpc>(envelope.rpc)?;
        let state = self.state.take().expect("state loaded");
        let snapshot = rmp_serde::to_vec_named(&state).map_err(internal)?;
        let sql = self.host.sql_handle();
        let now = self.host.now_ms();
        let transport = Rc::new(HostCaller {
            host: Rc::clone(&self.host),
        });
        let caller = Caller::new(me, envelope.chain.clone(), transport);
        let mut ctx = ServiceCtx::new(state, sql, now, caller);
        let result = self.service.on_call(&mut ctx, &envelope.from, rpc).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        match result {
            Ok(reply) => {
                self.persist_and_apply(effects).await.map_err(internal)?;
                rpc_to_value(reply)
            }
            Err(err) => {
                self.state = Some(rmp_serde::from_slice(&snapshot).map_err(internal)?);
                Err(CallError::Handler(format!("{err:#}")))
            }
        }
    }

    /// Read the persisted state as JSON, without creating anything. `None`
    /// means this service never ran.
    pub async fn inspect(&mut self) -> Result<Option<Value>> {
        if !self.has_meta()? {
            return Ok(None);
        }
        Ok(Some(serde_json::to_value(
            self.persisted_state()?.unwrap_or_default(),
        )?))
    }

    pub async fn alarm(&mut self) {
        if let Err(err) = self.handle_alarm().await {
            crate::runtime::log_error(err);
        }
    }

    async fn handle_alarm(&mut self) -> Result<()> {
        if self.ensure_loaded().await? {
            self.host.set_alarm(None).await?;
            return Ok(());
        }
        let now = self.host.now_ms();
        let rows = self.host.sql().exec(
            "SELECT key, due_ms FROM hearth_timers WHERE due_ms <= ? ORDER BY due_ms, key",
            vec![crate::host::SqlValue::Int(now as i64)],
        )?;
        let mut due = Vec::new();
        for row in rows {
            let Some(key) = row.get("key").and_then(Value::as_str) else {
                continue;
            };
            due.push(key.to_string());
        }
        for key in due {
            // Delete before firing so a failing handler can never loop forever.
            store::timer_delete(self.host.sql(), &key)?;
            if let Some(user_key) = key.strip_prefix(USER_TIMER_PREFIX) {
                self.run_timer(TimerKey(user_key.to_owned())).await?;
            }
        }
        self.reschedule_alarm().await
    }

    async fn run_timer(&mut self, key: TimerKey) -> Result<()> {
        let Some(state) = self.state.take() else {
            return Ok(());
        };
        let snapshot = rmp_serde::to_vec_named(&state)?;
        let sql = self.host.sql_handle();
        let now = self.host.now_ms();
        let transport = Rc::new(HostCaller {
            host: Rc::clone(&self.host),
        });
        let caller = Caller::new(self.target(""), Vec::new(), transport);
        let mut ctx = ServiceCtx::new(state, sql, now, caller);
        let result = self.service.on_timer(&mut ctx, key).await;
        let (state, effects) = ctx.into_parts();
        self.state = Some(state);
        match result {
            Ok(()) => self.persist_and_apply(effects).await,
            Err(err) => {
                // A failed timer handler is not retried: the row is already gone.
                self.state = Some(rmp_serde::from_slice(&snapshot)?);
                self.log_json(serde_json::json!({
                    "hearth": "handler_error",
                    "object": self.label(),
                    "msg": err.to_string(),
                }));
                Ok(())
            }
        }
    }

    /// Save the state and apply the handler's effects. Everything here is
    /// synchronous up to the first `await`, so one commit is one turn.
    async fn persist_and_apply(&mut self, effects: Vec<Effect>) -> Result<()> {
        self.persist()?;
        let now = self.host.now_ms();
        for effect in effects {
            match effect {
                Effect::Timer { key, after } => {
                    let due = now.saturating_add(after.as_millis() as u64);
                    store::timer_set(
                        self.host.sql(),
                        &format!("{USER_TIMER_PREFIX}{}", key.0),
                        due,
                    )?;
                }
                Effect::CancelTimer { key } => {
                    store::timer_delete(self.host.sql(), &format!("{USER_TIMER_PREFIX}{}", key.0))?;
                }
                // The room-only effects never appear in a service context.
                Effect::Event { .. }
                | Effect::Kick(_)
                | Effect::Lock
                | Effect::Dispose
                | Effect::Notify { .. } => {}
            }
        }
        self.reschedule_alarm().await
    }

    fn persist(&mut self) -> Result<()> {
        if self.incompatible {
            return Ok(());
        }
        let Some(state) = &self.state else {
            return Ok(());
        };
        let bytes = rmp_serde::to_vec_named(state)?;
        if self.last_saved.as_ref() == Some(&bytes) {
            return Ok(());
        }
        self.host.sql().exec(
            "INSERT OR REPLACE INTO hearth_state (id, seq, locked, data) VALUES (0, 0, 0, ?)",
            vec![crate::host::SqlValue::Blob(bytes.clone())],
        )?;
        self.last_saved = Some(bytes);
        Ok(())
    }

    async fn reschedule_alarm(&mut self) -> Result<()> {
        let next = store::next_timer(self.host.sql())?;
        self.host.set_alarm(next).await
    }

    /// Load state into memory, creating the fixed tables on first use. Returns
    /// whether the persisted state is incompatible with this build.
    async fn ensure_loaded(&mut self) -> Result<bool> {
        if self.state.is_some() {
            return Ok(self.incompatible);
        }
        if self.incompatible {
            return Ok(true);
        }
        self.host
            .sql()
            .exec(store::CREATE_STATE_TABLE, Vec::new())?;
        self.host
            .sql()
            .exec(store::CREATE_TIMERS_TABLE, Vec::new())?;
        self.host.sql().exec(store::CREATE_META_TABLE, Vec::new())?;
        if self.name.is_none() {
            self.name = store::meta_get(self.host.sql(), "name")?;
        }
        if let Some(state) = self.persisted_state()? {
            self.state = Some(state);
            Ok(false)
        } else {
            self.state = Some(S::State::default());
            self.persist()?;
            Ok(false)
        }
    }

    /// The persisted state, or `None` when no state row was written yet.
    fn persisted_state(&mut self) -> Result<Option<S::State>> {
        if !store::has_table(self.host.sql(), "hearth_state")? {
            return Ok(None);
        }
        let rows = self
            .host
            .sql()
            .exec("SELECT data FROM hearth_state WHERE id = 0", Vec::new())?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let data = store::json_to_bytes(row.get("data"))?;
        match rmp_serde::from_slice::<S::State>(&data) {
            Ok(state) => Ok(Some(state)),
            Err(_) => {
                self.incompatible = true;
                self.log_json(serde_json::json!({
                    "hearth": "state_incompatible",
                    "object": self.label(),
                }));
                Ok(None)
            }
        }
    }

    fn remember_name(&mut self, name: &str) -> std::result::Result<(), CallError> {
        store::meta_put_if_absent(self.host.sql(), "name", name).map_err(internal)?;
        self.name = Some(name.to_owned());
        Ok(())
    }

    fn has_meta(&self) -> Result<bool> {
        store::has_table(self.host.sql(), "hearth_meta")
    }

    fn label(&self) -> String {
        self.target("").name
    }

    fn log_json(&self, line: serde_json::Value) {
        self.host
            .log_line(&serde_json::to_string(&line).unwrap_or_default());
    }
}

/// Drop the in-memory runtime as hibernation does; storage stays.
impl<S: Service, H: Host + Clone + 'static> ServiceRuntime<S, H> {
    pub fn hibernate(&mut self) {
        let host = H::clone(&self.host);
        *self = Self::new(host, self.binding);
    }
}

/// Internal keys never collide with user timer keys: user timers are prefixed.
const USER_TIMER_PREFIX: &str = "u:";

fn internal(err: impl std::fmt::Display) -> CallError {
    CallError::Handler(err.to_string())
}
