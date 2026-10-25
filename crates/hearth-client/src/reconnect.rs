//! Sans-IO reconnect policy.
//!
//! After a socket drops, [`ReconnectPolicy::decide`] says whether to retry and
//! when. It is mirrored by `HearthReconnect.decide` in
//! `godot/addons/hearth/hearth_reconnect.gd`, and both are checked against
//! `tests/fixtures/reconnect_cases.json`.

/// The decision for a dropped socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconnect {
    /// Retry at this absolute clock value, in milliseconds.
    RetryAt(u64),
    /// Stop retrying: the close was deliberate or fatal, or the grace window is
    /// spent.
    GiveUp,
}

/// Exponential-backoff retry policy for a dropped socket.
#[derive(Clone, Copy, Debug)]
pub struct ReconnectPolicy {
    /// Give up once this long has passed since the first drop, in milliseconds.
    pub window_ms: u64,
    /// Delay before the first retry, in milliseconds.
    pub base_ms: u64,
    /// Retry delay cap, in milliseconds.
    pub max_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            window_ms: 30_000,
            base_ms: 500,
            max_ms: 8_000,
        }
    }
}

impl ReconnectPolicy {
    /// Decide what to do after `close_code` closed the socket, given that the
    /// socket first dropped at `first_drop_ms`, this is retry number `attempt`
    /// (0-based) and the clock now reads `now_ms`.
    pub fn decide(
        &self,
        close_code: u16,
        first_drop_ms: u64,
        attempt: u32,
        now_ms: u64,
    ) -> Reconnect {
        // 1000 is a deliberate close; 4000-4999 are application-fatal (for
        // example 4001 SCHEMA_MISMATCH), so retrying would spin forever.
        if close_code == 1000 || (4000..=4999).contains(&close_code) {
            return Reconnect::GiveUp;
        }
        if now_ms.saturating_sub(first_drop_ms) >= self.window_ms {
            return Reconnect::GiveUp;
        }
        let delay = self
            .base_ms
            .checked_shl(attempt)
            .unwrap_or(u64::MAX)
            .min(self.max_ms);
        Reconnect::RetryAt(now_ms.saturating_add(delay))
    }
}
