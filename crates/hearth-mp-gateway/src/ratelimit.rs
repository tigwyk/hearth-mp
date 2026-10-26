//! Rate limiting through the Workers Rate Limiting binding.
//!
//! The binding is optional: a deployment without it (local dev, a small game)
//! simply has no limits, and the gateway logs that once instead of failing.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use hearth_mp_protocol::PlayerId;
use worker::{console_log, Env, Response};

/// Which bucket a request is charged to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Shared by everyone behind one IP (CGNAT, schools): the generous limit.
    Ip,
    /// One player (one verified token): the tight limit.
    Player,
}

impl Scope {
    fn tag(self) -> &'static str {
        match self {
            Scope::Ip => "ip",
            Scope::Player => "player",
        }
    }
}

#[doc(hidden)]
/// The binding key for one request: `"ip:<ip>"` or `"p:<player_id>"`.
///
/// A player limit without a verified player has no key, so it is skipped.
pub fn key(scope: Scope, ip: &str, player: Option<&PlayerId>) -> Option<String> {
    match scope {
        Scope::Ip => (!ip.is_empty()).then(|| format!("ip:{ip}")),
        Scope::Player => player.map(|player| format!("p:{}", player.0)),
    }
}

/// The outcome of one limit check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// Under the limit.
    Ok,
    /// Over the limit.
    Limited {
        /// Seconds until the next period.
        retry_after_s: u32,
    },
    /// No such binding in this deployment: the request is allowed.
    NoBinding,
}

/// Where the limits live and how long their period is.
#[non_exhaustive]
pub struct Limits {
    /// The rate-limit binding for per-IP limits.
    pub ip_binding: &'static str,
    /// Its period, seconds.
    pub ip_period_s: u32,
    /// The rate-limit binding for per-player limits.
    pub player_binding: &'static str,
    /// Its period, seconds.
    pub player_period_s: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            ip_binding: "HEARTH_IP_LIMIT",
            // Generous on purpose: many players can share one IP.
            ip_period_s: 60,
            player_binding: "HEARTH_PLAYER_LIMIT",
            player_period_s: 60,
        }
    }
}

/// Charge one request to `key`. A missing binding allows the request.
pub async fn check(env: &Env, binding: &str, key: &str, period_s: u32) -> Limit {
    let limiter = match env.rate_limiter(binding) {
        Ok(limiter) => limiter,
        Err(_) => {
            log_disabled_once(binding);
            return Limit::NoBinding;
        }
    };
    match limiter.limit(key.to_owned()).await {
        Ok(outcome) if outcome.success => Limit::Ok,
        Ok(_) => Limit::Limited {
            retry_after_s: period_s,
        },
        Err(err) => {
            // A failed check must not lock players out of the game.
            console_log!(
                "{}",
                serde_json::json!({
                    "hearth": "ratelimit_error",
                    "binding": binding,
                    "msg": err.to_string(),
                })
            );
            Limit::Ok
        }
    }
}

#[doc(hidden)]
/// The 429 for a limited request, as data so it can be tested natively:
/// `(status, headers, JSON body)`.
pub fn response_for(scope: Scope, retry_after_s: u32) -> (u16, Vec<(String, String)>, String) {
    let body = serde_json::json!({ "error": "rate_limited", "scope": scope.tag() });
    (
        429,
        vec![
            ("Retry-After".to_owned(), retry_after_s.to_string()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ],
        body.to_string(),
    )
}

#[doc(hidden)]
/// Build the Workers response for a limited request.
pub fn limited(scope: Scope, retry_after_s: u32) -> worker::Result<Response> {
    let (status, headers, body) = response_for(scope, retry_after_s);
    let response = Response::ok(body)?.with_status(status);
    for (name, value) in headers {
        response.headers().set(&name, &value)?;
    }
    Ok(response)
}

fn log_disabled_once(binding: &str) {
    static LOGGED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let logged = LOGGED.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut logged) = logged.lock() else {
        return;
    };
    if logged.insert(binding.to_owned()) {
        console_log!(
            "{}",
            serde_json::json!({
                "hearth": "ratelimit_disabled",
                "binding": binding,
            })
        );
    }
}
