//! Typed Durable-Object-to-Durable-Object calls.
//!
//! Every Hearth object (a [`crate::Room`], a [`crate::Service`]) is reached by
//! `POST /__hearth/call` with a MessagePack [`Envelope`]. The envelope carries
//! the caller, the call chain and the JSON-shaped rpc payload; the reply is a
//! MessagePack `Result<Value, CallError>`.
//!
//! The chain is what keeps a reentrant cycle from hanging: object A calls B,
//! B calls A, while A is still awaiting. Each object is single-threaded, so the
//! inner call would wait forever for a turn that cannot come. Instead every
//! receiver checks the chain *before* taking its runtime lock and answers
//! [`CallError::ReentrantCall`] at once.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use hearth_mp_protocol::Value;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::host::Host;

/// One addressable Hearth object: the Durable Object binding name plus the
/// object's name (the `id_from_name` name).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    /// The Durable Object binding name (`wrangler.toml`).
    pub binding: String,
    /// The object's name within the binding (its `id_from_name` name).
    pub name: String,
}

impl Target {
    /// A target from its binding and name.
    pub fn new(binding: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            binding: binding.into(),
            name: name.into(),
        }
    }
}

/// One object-to-object call.
///
/// `chain` lists every object currently awaiting an answer, outermost first. A
/// receiver that finds itself in the chain answers [`CallError::ReentrantCall`]
/// instead of deadlocking.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Envelope {
    /// Set for durable `notify` deliveries only; the receiver deduplicates it.
    pub msg_id: Option<String>,
    /// The calling object.
    pub from: Target,
    /// Every object awaiting an answer, outermost first (reentrancy detection).
    pub chain: Vec<Target>,
    /// The rpc payload, JSON-shaped like every other Hearth payload.
    pub rpc: Value,
}

/// A failed call. Serialises into the reply body, so the caller sees the exact
/// failure the receiver produced.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CallError {
    #[error("reentrant call into {0:?}")]
    /// The target is already waiting on this call chain; answering would deadlock.
    ReentrantCall(Target),
    #[error("bad rpc: {0}")]
    /// The rpc payload did not decode into the receiver's `Rpc` type.
    BadRpc(String),
    #[error("{0}")]
    /// The receiver's handler returned this error.
    Handler(String),
    #[error("unreachable: {0}")]
    /// The target could not be reached or did not answer; the only retryable error.
    Unreachable(String),
}

impl CallError {
    /// Only a target that could not answer at all is worth retrying: a handler
    /// that ran and rejected the call (or a bad payload) never will be.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }
}

/// Compile-time descriptor of a callable object: the binding that reaches it and
/// the types its `on_call` speaks. `#[hearth_mp::service]` and `#[hearth_mp::room(...,
/// rpc)]` emit it; a native test writes the same three lines by hand.
pub trait Callable {
    /// The Durable Object binding that reaches this object.
    const BINDING: &'static str;
    /// What its `on_call` accepts.
    type Rpc: Serialize + DeserializeOwned;
    /// What its `on_call` answers.
    type Reply: Serialize + DeserializeOwned;
}

/// A boxed call future. One Durable Object runs one turn at a time, so the
/// future is not `Send`, which keeps `Ctx` free of a `Send` bound.
pub type CallFuture<'a> = Pin<Box<dyn Future<Output = std::result::Result<Value, CallError>> + 'a>>;

/// How a runtime reaches another object. The [`Host`] implements it through
/// [`HostCaller`]; tests route it through their in-memory world.
pub trait CallTransport {
    /// Send one call to `to` and await the reply.
    fn call(&self, to: Target, envelope: Envelope) -> CallFuture<'_>;
}

/// [`CallTransport`] over a [`Host`], holding only an `Rc` so a [`Ctx`] can
/// outlive the runtime borrow that produced it.
pub(crate) struct HostCaller<H: Host> {
    pub host: Rc<H>,
}

impl<H: Host> CallTransport for HostCaller<H> {
    fn call(&self, to: Target, envelope: Envelope) -> CallFuture<'_> {
        Box::pin(async move { self.host.call(&to, envelope).await })
    }
}

/// Everything a handler needs to issue object-to-object calls: who it is, which
/// objects are already waiting on it, and how to reach the platform.
#[derive(Clone)]
pub(crate) struct Caller {
    me: Target,
    chain: Vec<Target>,
    transport: Rc<dyn CallTransport>,
}

impl Caller {
    pub fn new(me: Target, chain: Vec<Target>, transport: Rc<dyn CallTransport>) -> Self {
        Self {
            me,
            chain,
            transport,
        }
    }

    pub fn me(&self) -> &Target {
        &self.me
    }

    /// The envelope for a call this object is making: the incoming chain plus
    /// this object, so a target that is already awaiting is caught.
    fn envelope(&self, rpc: Value, msg_id: Option<String>) -> Envelope {
        let mut chain = self.chain.clone();
        chain.push(self.me.clone());
        Envelope {
            msg_id,
            from: self.me.clone(),
            chain,
            rpc,
        }
    }

    /// Send the call now. It is **not** rolled back when the handler later
    /// fails; use `ctx.notify` for work that has to follow the commit.
    pub async fn call<T: Callable>(
        &self,
        name: &str,
        rpc: T::Rpc,
    ) -> std::result::Result<T::Reply, CallError> {
        let to = Target::new(T::BINDING, name);
        let envelope = self.envelope(rpc_to_value(rpc)?, None);
        let reply = self.transport.call(to, envelope).await?;
        serde_json::from_value(reply).map_err(|err| {
            CallError::BadRpc(format!(
                "reply did not decode into {}: {err}",
                std::any::type_name::<T::Reply>()
            ))
        })
    }
}

/// Serialise an rpc payload to its JSON form. A payload that does not serialise
/// is a dev error, not a transport failure.
pub(crate) fn rpc_to_value(rpc: impl Serialize) -> std::result::Result<Value, CallError> {
    serde_json::to_value(rpc).map_err(|err| CallError::BadRpc(err.to_string()))
}

/// Decode an rpc payload, naming the type that failed.
pub(crate) fn rpc_from_value<T: DeserializeOwned>(
    value: Value,
) -> std::result::Result<T, CallError> {
    serde_json::from_value(value).map_err(|err| {
        CallError::BadRpc(format!(
            "rpc did not decode into {}: {err}",
            std::any::type_name::<T>()
        ))
    })
}
