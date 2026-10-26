//! Rust SDK for Hearth dedicated handoff servers.
//!
//! The allocator (or the container runtime) starts a dedicated server with three
//! values: the session id, the session secret and the report base URL. This
//! crate turns them into the server's side of the handoff protocol:
//!
//! - [`Session::redeem`] exchanges a player's single-use ticket for that
//!   player's identity. The identity, not the ticket, is what the server then
//!   trusts for the connection.
//! - [`Session::report`] reports lifecycle events (`Started`, `PlayerJoined`,
//!   `Result`, `Ended`, ...) back to the room, signed with the session secret.
//! - [`Session::spawn_heartbeat`] keeps the room's "the server is alive" check
//!   fed while the match runs; `Ended` or a `Gone` answer stops it.
//!
//! Everything is blocking (`ureq`): a dedicated server is a normal process, not
//! a Worker. A game server that already has an async runtime can run these calls
//! on a blocking task.

#![warn(missing_docs)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub use hearth_server::handoff::{HandoffError, Redeemed, ServerReport};

/// The default heartbeat cadence. The room's heartbeat timeout is 60 s.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(20);
/// Outbound HTTP calls give up after this long.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a redeem or a report failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerError {
    /// Bad secret, used or expired ticket, or a ticket from another session.
    /// The room deliberately does not say which check failed.
    Forbidden,
    /// The session has ended or failed.
    Gone,
    /// No such session.
    NotFound,
    /// The HTTP call failed, or the answer was not understood.
    Http(String),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden => formatter.write_str("forbidden"),
            Self::Gone => formatter.write_str("session is gone"),
            Self::NotFound => formatter.write_str("no such session"),
            Self::Http(message) => write!(formatter, "http error: {message}"),
        }
    }
}

impl std::error::Error for ServerError {}

/// One dedicated server's handoff credentials.
///
/// The fields come from the environment contract (`HEARTH_SID`,
/// `HEARTH_SESSION_SECRET`, `HEARTH_REPORT_URL`), or from `Session::new` in
/// tests. The stopped flag is shared with the heartbeat thread, so it is private
/// and the constructors below are the way to build a session.
#[derive(Clone)]
pub struct Session {
    /// `HEARTH_SID`: the session id.
    pub sid: String,
    /// `HEARTH_SESSION_SECRET`: authenticates redeems and reports.
    pub secret: String,
    /// `<public_url>/handoff/<sid>`, as the allocator reported it.
    pub report_url: String,
    /// Set once `Ended` is reported or the room answers `Gone`.
    stopped: Arc<AtomicBool>,
}

impl Session {
    /// A session from its id, secret and report URL (tests; servers use `from_env`).
    pub fn new(
        sid: impl Into<String>,
        secret: impl Into<String>,
        report_url: impl Into<String>,
    ) -> Self {
        Self {
            sid: sid.into(),
            secret: secret.into(),
            report_url: report_url.into(),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Read the container/allocator environment contract: `HEARTH_SID`,
    /// `HEARTH_SESSION_SECRET` and `HEARTH_REPORT_URL`.
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| -> Result<String, String> {
            std::env::var(name).map_err(|_| format!("{name} is not set"))
        };
        Ok(Self::new(
            var("HEARTH_SID")?,
            var("HEARTH_SESSION_SECRET")?,
            var("HEARTH_REPORT_URL")?,
        ))
    }

    /// Exchange a player's single-use ticket for that player's identity.
    pub fn redeem(&self, ticket: &str) -> Result<Redeemed, ServerError> {
        let mut response = self.post("redeem", serde_json::json!({ "ticket": ticket }))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(status_error(status));
        }
        response
            .body_mut()
            .read_json::<Redeemed>()
            .map_err(|error| ServerError::Http(error.to_string()))
    }

    /// Report one lifecycle event.
    ///
    /// Reporting `Ended`, or getting `Gone` from the room, stops the heartbeat
    /// thread: the session is over either way.
    pub fn report(&self, report: &ServerReport) -> Result<(), ServerError> {
        let outcome = self.report_once(report);
        if matches!(report, ServerReport::Ended { .. })
            || matches!(&outcome, Err(ServerError::Gone))
        {
            self.stopped.store(true, Ordering::SeqCst);
        }
        outcome
    }

    /// Send [`ServerReport::Heartbeat`] every `every` until the session ends or
    /// the room answers `Gone`. The join handle lets a server await shutdown.
    pub fn spawn_heartbeat(&self, every: Duration) -> std::thread::JoinHandle<()> {
        let session = self.clone();
        std::thread::spawn(move || {
            while !session.stopped.load(Ordering::SeqCst) {
                std::thread::sleep(every);
                if session.stopped.load(Ordering::SeqCst) {
                    break;
                }
                // A `Gone` answer sets `stopped`, so the loop ends after this.
                let _ = session.report(&ServerReport::Heartbeat);
            }
        })
    }

    fn report_once(&self, report: &ServerReport) -> Result<(), ServerError> {
        let body =
            serde_json::to_value(report).map_err(|error| ServerError::Http(error.to_string()))?;
        let response = self.post("report", body)?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(status_error(status))
        }
    }

    /// POST one JSON body to `<report_url>/<action>` with the session secret.
    fn post(
        &self,
        action: &str,
        body: serde_json::Value,
    ) -> Result<ureq::http::Response<ureq::Body>, ServerError> {
        let url = format!("{}/{action}", self.report_url.trim_end_matches('/'));
        let request = ureq::post(&url)
            .config()
            .timeout_global(Some(HTTP_TIMEOUT))
            .build()
            .header("X-Hearth-Session-Secret", &self.secret)
            .header("Content-Type", "application/json");
        match request.send_json(body) {
            Ok(response) => Ok(response),
            // ureq maps 4xx/5xx to `StatusCode` by default (the body is not part
            // of the error, and the room's errors deliberately say nothing more).
            Err(ureq::Error::StatusCode(status)) => Err(status_error(status)),
            Err(error) => Err(ServerError::Http(error.to_string())),
        }
    }
}

fn status_error(status: u16) -> ServerError {
    match status {
        403 => ServerError::Forbidden,
        404 => ServerError::NotFound,
        410 => ServerError::Gone,
        other => ServerError::Http(format!("unexpected HTTP status {other}")),
    }
}
