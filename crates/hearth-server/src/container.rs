//! Cloudflare Containers adapter for handoff sessions.
//!
//! One Durable Object instance owns one session's container (the room loader
//! names the object after the session id, so the two are 1:1). The room's job
//! runner sends [`ContainerRpc`] messages to [`ContainerObject`], which starts
//! the container, waits for its `/healthz` through the container's TCP port,
//! forwards player WebSocket connections to it, and destroys it on stop or on a
//! backstop alarm.
//!
//! A class is defined in the Worker crate with [`container_object!`], and the
//! same class must be listed in `wrangler.toml` twice: as a Durable Object
//! binding for the room adapter and under `[[containers]]` so the runtime
//! attaches a container to it.
//!
//! Everything here is behind `hearth-server`'s `worker` feature; the
//! host-independent [`crate::allocator::ContainerAllocator`] calls it through
//! [`crate::allocator::ContainerStarter`].

use std::time::Duration;

use serde::{Deserialize, Serialize};
use worker::js_sys::{Function, Object, Reflect};
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::{
    Container, ContainerStartupOptions, Date, DateInit, Delay, Env, Headers, Method, Request,
    RequestInit, Response, Result, ScheduledTime, State,
};

use crate::allocator::DEFAULT_CONTAINER_PORT;

/// Storage key of the current session's record.
const SESSION_KEY: &str = "container-session";
/// How often a start polls the container's `/healthz`.
const HEALTH_POLL: Duration = Duration::from_millis(500);
/// How long a start waits for `/healthz` before giving up.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
/// Extra time after a session's max duration before the backstop alarm fires.
const BACKSTOP_GRACE_MS: u64 = 5 * 60 * 1000;
/// The environment variables the session owns. A developer's `env` cannot set
/// any `HEARTH_*` name, so a container can never be pointed at another session's
/// report URL or overridden secret.
const RESERVED_ENV_PREFIX: &str = "HEARTH_";

/// One request from the room adapter to a session's container object.
///
/// The session secret travels only in `Start`, never in a client-visible value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ContainerRpc {
    /// Start the container for `sid` and wait until it is healthy.
    Start {
        /// The session id.
        sid: String,
        /// The plaintext session secret.
        secret: String,
        /// Where the container reports lifecycle events.
        report_url: String,
        /// The port the container serves HTTP/WebSocket on (0 means the default).
        port: u16,
        /// The developer's extra environment; `HEARTH_*` entries are ignored.
        env: Vec<(String, String)>,
        /// The container is destroyed this long after it starts, at the latest.
        max_duration_ms: u64,
    },
    /// Destroy the container and forget the session.
    Stop,
}

/// The persisted session record: enough to proxy after a hibernation and to
/// schedule the backstop alarm.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionRecord {
    sid: String,
    started_ms: u64,
    max_duration_ms: u64,
    port: u16,
}

/// The Durable Object body that owns one session's container. Use
/// [`container_object!`] in the Worker crate to define the class itself.
pub struct ContainerObject {
    state: State,
}

impl ContainerObject {
    /// The environment is accepted for symmetry with the other object types;
    /// everything the container needs arrives in [`ContainerRpc::Start`].
    pub fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    /// `POST /__hearth/call` runs a [`ContainerRpc`]; `/__hearth/proxy/*` forwards
    /// a player connection (including a WebSocket upgrade) to the container.
    pub async fn fetch(&self, mut req: Request) -> Result<Response> {
        let path = req.path();
        if req.method() == Method::Post && path == "/__hearth/call" {
            let body = req.bytes().await?;
            let rpc: ContainerRpc = match serde_json::from_slice(&body) {
                Ok(rpc) => rpc,
                Err(_) => return Response::error("invalid container rpc", 400),
            };
            return match rpc {
                ContainerRpc::Start {
                    sid,
                    secret,
                    report_url,
                    port,
                    env,
                    max_duration_ms,
                } => {
                    self.start(sid, secret, report_url, port, env, max_duration_ms)
                        .await
                }
                ContainerRpc::Stop => {
                    self.destroy().await?;
                    Response::empty().map(|response| response.with_status(204))
                }
            };
        }
        if path.starts_with("/__hearth/proxy") {
            return self.proxy(req).await;
        }
        Response::error("not found", 404)
    }

    /// Backstop: a container is destroyed when its session's max duration is up,
    /// even if the room never sent a stop.
    pub async fn alarm(&self) -> Result<Response> {
        self.destroy().await?;
        Response::empty()
    }

    async fn start(
        &self,
        sid: String,
        secret: String,
        report_url: String,
        port: u16,
        env: Vec<(String, String)>,
        max_duration_ms: u64,
    ) -> Result<Response> {
        let port = if port == 0 {
            DEFAULT_CONTAINER_PORT
        } else {
            port
        };
        let Some(container) = self.state.container() else {
            return Response::error(
                "containers are not enabled for this Durable Object; add a [[containers]] entry with this class_name",
                501,
            );
        };
        let current: Option<SessionRecord> = self.state.storage().get(SESSION_KEY).await?;
        if container.running() {
            // A start for the session that is already up is the idempotent retry
            // path (an allocation attempt may be re-issued after a hibernation).
            if current.as_ref().is_some_and(|record| record.sid == sid) {
                return Response::empty();
            }
            // Anything else is a stale container from a previous session.
            container.destroy(Some("replaced by a new start")).await?;
        }

        let mut env_pairs: Vec<(String, String)> = vec![
            ("HEARTH_SID".to_owned(), sid.clone()),
            ("HEARTH_SESSION_SECRET".to_owned(), secret),
            ("HEARTH_REPORT_URL".to_owned(), report_url),
            ("HEARTH_PORT".to_owned(), port.to_string()),
        ];
        for (key, value) in &env {
            if !key.starts_with(RESERVED_ENV_PREFIX) {
                env_pairs.push((key.clone(), value.clone()));
            }
        }
        start_container(&container, &env_pairs)?;

        let started_ms = Date::now().as_millis();
        let record = SessionRecord {
            sid,
            started_ms,
            max_duration_ms,
            port,
        };
        self.state.storage().put(SESSION_KEY, &record).await?;
        // Persist before polling so a failure mid-poll still leaves the backstop.
        let backstop_ms = record.started_ms + record.max_duration_ms + BACKSTOP_GRACE_MS;
        self.state
            .storage()
            .set_alarm(scheduled(backstop_ms))
            .await?;

        let deadline_ms = started_ms + HEALTH_TIMEOUT.as_millis() as u64;
        loop {
            if healthy(&container, port).await {
                return Response::empty();
            }
            if Date::now().as_millis() >= deadline_ms {
                let _ = container.destroy(Some("health check timed out")).await;
                let _ = self.state.storage().delete(SESSION_KEY).await;
                let _ = self.state.storage().delete_alarm().await;
                return Response::error("container did not become healthy", 504);
            }
            Delay::from(HEALTH_POLL).await;
        }
    }

    /// Forward a player connection to the container's TCP port, untouched. The
    /// request carries the gateway's identity headers and, for a WebSocket, the
    /// upgrade headers; the returned 101 response carries the live socket.
    async fn proxy(&self, req: Request) -> Result<Response> {
        let record: Option<SessionRecord> = self.state.storage().get(SESSION_KEY).await?;
        let Some(record) = record else {
            return Response::error("no container session", 404);
        };
        let Some(container) = self.state.container() else {
            return Response::error("containers are not enabled for this Durable Object", 501);
        };
        if !container.running() {
            return Response::error("container is not running", 410);
        }
        let fetcher = container.get_tcp_port(record.port)?;
        // Containers only accept plain HTTP from the edge: `https://container`
        // is rejected ("Connecting to a container using HTTPS is not currently
        // supported"). The client hop is already TLS, so rebuild the request
        // against the http:// host, carrying the identity and WebSocket upgrade
        // headers unchanged. The proxy is only used for WebSocket upgrades, so
        // there is no body to forward.
        let headers = Headers::new();
        for (name, value) in req.headers().entries() {
            headers.set(&name, &value)?;
        }
        let mut target = format!("http://container{}", req.path());
        if let Some(query) = req
            .url()
            .ok()
            .and_then(|url| url.query().map(str::to_owned))
        {
            target.push('?');
            target.push_str(&query);
        }
        let mut init = RequestInit::new();
        init.with_method(req.method()).with_headers(headers);
        let request = Request::new_with_init(&target, &init)?;
        fetcher.fetch_request(request).await
    }

    /// Destroy the container (if any) and forget the session.
    async fn destroy(&self) -> Result<()> {
        if let Some(container) = self.state.container() {
            if container.running() {
                container.destroy(Some("session ended")).await?;
            }
        }
        let _ = self.state.storage().delete(SESSION_KEY).await;
        let _ = self.state.storage().delete_alarm().await;
        Ok(())
    }
}

/// `GET http://container/healthz` through the container's TCP port.
async fn healthy(container: &Container, port: u16) -> bool {
    let Ok(fetcher) = container.get_tcp_port(port) else {
        return false;
    };
    matches!(
        fetcher.fetch("http://container/healthz", None).await,
        Ok(response) if response.status_code() == 200
    )
}

fn scheduled(at_ms: u64) -> ScheduledTime {
    let at: worker::js_sys::Date = Date::from(DateInit::Millis(at_ms)).into();
    ScheduledTime::new(at)
}

/// Start the container with the session environment.
///
/// `worker` 0.8.7's `From<ContainerStartupOptions> for Object` *deletes* every
/// non-empty option (`env`, `entrypoint`, `enableInternet`) before the runtime
/// sees it, so the typed `start(Some(options))` silently drops the env and the
/// dedicated server starts without `HEARTH_SID`. Its typed `env` is also a
/// `js_sys::Map`, while the runtime API takes a plain `Record<string, string>`.
/// For the non-empty case, build the JS options object the runtime documents and
/// call `start` directly; the typed path stays for an empty start.
///
/// Verified against the deployed Worker: with the runtime-shaped object the
/// container has the session env and the room receives the full match lifecycle
/// (started, player joined, result, ended).
fn start_container(container: &Container, env: &[(String, String)]) -> Result<()> {
    if env.is_empty() {
        return container.start(Some(ContainerStartupOptions::new()));
    }
    let options = Object::new();
    let env_object = Object::new();
    for (key, value) in env {
        Reflect::set(
            &env_object,
            &JsValue::from_str(key),
            &JsValue::from_str(value),
        )
        .map_err(worker::Error::from)?;
    }
    Reflect::set(&options, &JsValue::from_str("env"), env_object.as_ref())
        .map_err(worker::Error::from)?;
    // Required whenever a startup options object is passed, and the report
    // requests must reach the Worker, so the container needs outbound access.
    Reflect::set(
        &options,
        &JsValue::from_str("enableInternet"),
        &JsValue::TRUE,
    )
    .map_err(worker::Error::from)?;
    let start = Reflect::get(container.as_ref(), &JsValue::from_str("start"))
        .map_err(worker::Error::from)?;
    let start: Function = start
        .dyn_into()
        .map_err(|_| worker::Error::RustError("container start is not a JS function".to_owned()))?;
    start
        .call1(container.as_ref(), options.as_ref())
        .map_err(worker::Error::from)?;
    Ok(())
}

/// Define the Durable Object class that owns one session's container.
///
/// Invoke once at the crate root of a Worker that enables `hearth-server`'s
/// `worker` feature:
///
/// ```ignore
/// hearth::server::container_object!(ArenaServer);
/// ```
///
/// Then list `ArenaServer` in `wrangler.toml` as a Durable Object binding (for
/// the room adapter's job runner) and under `[[containers]]` with
/// `class_name = "ArenaServer"`.
#[macro_export]
macro_rules! container_object {
    ($class:ident) => {
        // `#[worker::durable_object]`'s wasm-bindgen struct-marker expansion
        // spells the crate as bare `wasm_bindgen`, so the wrapper lives in a
        // hidden module that puts that name in scope.
        #[doc(hidden)]
        pub mod __hearth_container_object {
            #[allow(unused_imports)]
            use ::worker::wasm_bindgen;

            #[::worker::durable_object]
            pub struct $class($crate::ContainerObject);

            impl ::worker::DurableObject for $class {
                fn new(state: ::worker::State, env: ::worker::Env) -> Self {
                    Self($crate::ContainerObject::new(state, env))
                }

                async fn fetch(
                    &self,
                    req: ::worker::Request,
                ) -> ::worker::Result<::worker::Response> {
                    self.0.fetch(req).await
                }

                async fn alarm(&self) -> ::worker::Result<::worker::Response> {
                    self.0.alarm().await
                }
            }
        }

        pub use self::__hearth_container_object::$class;
    };
}
