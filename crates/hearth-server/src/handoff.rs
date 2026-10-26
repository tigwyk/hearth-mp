//! Handoff sessions: the vocabulary shared by the room runtime, the Durable
//! Object adapter, the gateway and the Rust server SDK.
//!
//! A handoff is a room capability, not a room mode. A room calls
//! [`crate::Ctx::handoff`] with a [`HandoffRequest`]; the runtime records a
//! session and an allocation job in the room's own SQLite as part of the
//! handler's commit, and the adapter runs the allocation outside the runtime
//! lock. On success every player receives a private `hearth.handoff` event with
//! the endpoint and a single-use ticket ([`new_ticket`]), and the dedicated
//! server reports lifecycle events back through [`ServerReport`].
//!
//! Security shape:
//!
//! - Tickets are 16 random bytes as 32 hex characters, valid for
//!   [`TICKET_TTL`], single use, bound to one `(session, player)`. The room
//!   stores only [`hash_ticket`] of each.
//! - Each session gets a 32-byte secret as 64 hex characters; only
//!   [`hash_secret`] is stored. The plaintext goes to the allocator (or the
//!   container) and never to a client.
//! - All comparisons of hashes use [`constant_time_eq`].

use std::fmt;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hearth_protocol::{PlayerId, Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::host::RoomName;

/// How long a minted ticket stays valid.
pub const TICKET_TTL: Duration = Duration::from_secs(60);
/// Default time to report `started` after the endpoint is known.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(60);
/// Default maximum gap between server reports once started.
pub const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);
/// Default hard cap on a session's life.
pub const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(2 * 60 * 60);
/// Allocation attempts before the session fails.
pub const MAX_ALLOC_ATTEMPTS: u32 = 3;
/// How long a job may stay in flight before it is re-issued.
pub const JOB_IN_FLIGHT: Duration = Duration::from_secs(30);
/// How long ended and failed sessions are kept before pruning.
pub const SESSION_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// The event that carries a player's private handoff ticket.
pub const HANDOFF_EVENT: &str = "hearth.handoff";
/// The event that tells a player the session is over.
pub const HANDOFF_ENDED_EVENT: &str = "hearth.handoff_ended";
/// Framework-owned event names use this prefix; rooms may not send them.
pub const RESERVED_EVENT_PREFIX: &str = "hearth.";

pub const CREATE_SESSIONS_TABLE: &str = "CREATE TABLE IF NOT EXISTS hearth_sessions (\
    sid TEXT PRIMARY KEY, \
    status TEXT NOT NULL, \
    secret_hash BLOB NOT NULL, \
    players TEXT NOT NULL, \
    claims TEXT NOT NULL, \
    region TEXT, \
    endpoint TEXT, \
    attempts INTEGER NOT NULL, \
    created_ms INTEGER, \
    deadline_ms INTEGER, \
    ended_ms INTEGER, \
    start_timeout_ms INTEGER NOT NULL, \
    heartbeat_timeout_ms INTEGER NOT NULL, \
    max_duration_ms INTEGER NOT NULL)";

pub const CREATE_TICKETS_TABLE: &str = "CREATE TABLE IF NOT EXISTS hearth_tickets (\
    hash BLOB PRIMARY KEY, \
    sid TEXT NOT NULL, \
    player_id TEXT NOT NULL, \
    exp_ms INTEGER NOT NULL, \
    used INTEGER NOT NULL)";

/// A session's persisted lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Allocating,
    Ready,
    Started,
    Ended,
    Failed,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allocating => "allocating",
            Self::Ready => "ready",
            Self::Started => "started",
            Self::Ended => "ended",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "allocating" => Some(Self::Allocating),
            "ready" => Some(Self::Ready),
            "started" => Some(Self::Started),
            "ended" => Some(Self::Ended),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// Whether the session still keeps the room alive and accepts reports.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Allocating | Self::Ready | Self::Started)
    }
}

/// A handoff session's public identity.
///
/// The value is `hs_<base64url-nopad(room_type/name)>_<16 hex>`: the gateway can
/// recover the room from the id alone (see [`SessionId::room`]), and the hex
/// suffix makes ids unguessable.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// A fresh id for a session in `room`.
    pub fn new(room: &RoomName) -> Self {
        let payload = URL_SAFE_NO_PAD.encode(format!("{}/{}", room.room_type, room.name));
        Self(format!("hs_{payload}_{}", hex_encode(&random_bytes(8))))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The room encoded in the id. The gateway uses this to route a ticket
    /// redemption or a server report without trusting the caller.
    pub fn room(&self) -> Option<RoomName> {
        let rest = self.0.strip_prefix("hs_")?;
        let (payload, suffix) = rest.rsplit_once('_')?;
        if suffix.len() != 16 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        let (room_type, name) = text.split_once('/')?;
        if room_type.is_empty() || name.is_empty() {
            return None;
        }
        Some(RoomName {
            room_type: room_type.to_owned(),
            name: name.to_owned(),
        })
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a room asks for when it hands players to a dedicated server.
#[derive(Clone, Debug)]
pub struct HandoffRequest {
    pub players: Vec<PlayerId>,
    /// Game-defined data for the dedicated server (mode, map, rules, ...).
    pub claims: Value,
    pub region: Option<String>,
    pub start_timeout: Duration,
    pub heartbeat_timeout: Duration,
    pub max_duration: Duration,
}

impl HandoffRequest {
    pub fn new(players: Vec<PlayerId>, claims: Value) -> Self {
        Self {
            players,
            claims,
            region: None,
            start_timeout: DEFAULT_START_TIMEOUT,
            heartbeat_timeout: DEFAULT_HEARTBEAT_TIMEOUT,
            max_duration: DEFAULT_MAX_DURATION,
        }
    }

    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    pub fn start_timeout(mut self, timeout: Duration) -> Self {
        self.start_timeout = timeout;
        self
    }

    pub fn heartbeat_timeout(mut self, timeout: Duration) -> Self {
        self.heartbeat_timeout = timeout;
        self
    }

    pub fn max_duration(mut self, duration: Duration) -> Self {
        self.max_duration = duration;
        self
    }
}

/// One allocation attempt, handed to the adapter to run outside the runtime lock.
#[derive(Clone, Debug)]
pub struct AllocJob {
    pub sid: SessionId,
    /// The plaintext session secret. It only lives in memory until allocation
    /// succeeds; before that the dedicated server has never seen it, so a
    /// restart may regenerate it.
    pub secret: String,
    pub room: RoomName,
    pub players: Vec<PlayerId>,
    pub claims: Value,
    pub region: Option<String>,
    /// 1 for the first attempt.
    pub attempt: u32,
}

/// A successfully redeemed ticket, as the dedicated server sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Redeemed {
    pub player_id: PlayerId,
    pub provider: String,
    pub display_name: Option<String>,
    pub claims: Value,
    pub sid: SessionId,
}

/// Lifecycle events a dedicated server reports back to the room.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerReport {
    Started,
    Heartbeat,
    PlayerJoined { player: PlayerId },
    PlayerLeft { player: PlayerId },
    Result { data: Value },
    Ended { reason: String },
}

/// What the room's [`crate::Room::on_handoff`] hook receives.
#[derive(Clone, Debug, PartialEq)]
pub enum HandoffEvent {
    Ready { endpoint: String },
    Started,
    PlayerJoined(PlayerId),
    PlayerLeft(PlayerId),
    Result(Value),
    Ended { reason: String },
    Failed { reason: String },
}

/// Why a redeem or report was rejected. The variants are the only information a
/// server gets: `Forbidden` covers a bad secret, a used or expired ticket, and a
/// ticket minted for another session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffError {
    NotFound,
    Forbidden,
    Gone,
}

impl HandoffError {
    /// The HTTP status an internal endpoint or the gateway answers with.
    pub fn status(self) -> u16 {
        match self {
            Self::NotFound => 404,
            Self::Forbidden => 403,
            Self::Gone => 410,
        }
    }
}

impl fmt::Display for HandoffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotFound => "not found",
            Self::Forbidden => "forbidden",
            Self::Gone => "gone",
        })
    }
}

impl std::error::Error for HandoffError {}

/// Whether an event name is reserved for the framework.
pub fn is_reserved_event(name: &str) -> bool {
    name.starts_with(RESERVED_EVENT_PREFIX)
}

/// Mint a ticket: returns `(plaintext, hash)`. Only the hash is stored.
pub fn new_ticket() -> (String, Vec<u8>) {
    let value = hex_encode(&random_bytes(16));
    let hash = hash_ticket(&value);
    (value, hash)
}

/// The stored form of a ticket.
pub fn hash_ticket(ticket: &str) -> Vec<u8> {
    Sha256::digest(ticket.as_bytes()).to_vec()
}

/// Mint a session secret: returns `(plaintext hex, hash)`. Only the hash is
/// stored, and the plaintext is sent to the allocator or container once.
pub fn new_secret() -> (String, Vec<u8>) {
    let value = hex_encode(&random_bytes(32));
    let hash = hash_secret(&value);
    (value, hash)
}

/// The stored form of a session secret.
pub fn hash_secret(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

/// Compare two hashes without leaking where they differ.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    // A server that cannot reach its OS RNG cannot mint secrets or tickets
    // safely, so failing loudly is the only safe behaviour.
    getrandom::getrandom(&mut bytes).expect("hearth: OS random source failed");
    bytes
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn room() -> RoomName {
        RoomName {
            room_type: "tictactoe".to_owned(),
            name: "m_enam_0123456789abcdef".to_owned(),
        }
    }

    #[test]
    fn session_id_round_trips_its_room() {
        let name = room();
        let sid = SessionId::new(&name);
        assert_eq!(sid.room(), Some(name));
        assert!(sid.as_str().starts_with("hs_"));
        assert_eq!(
            sid.as_str().rsplit_once('_').expect("suffix").1.len(),
            16,
            "16 hex characters of randomness"
        );
        assert_eq!(SessionId::new(&room()).as_str() != sid.as_str(), true);
    }

    #[test]
    fn malformed_session_ids_are_rejected() {
        for bad in [
            "",
            "hs_",
            "not-a-sid",
            "hs_abc",
            "hs_unpadded_short",
            "hs_AAAA_0123456789abcdef_extra",
        ] {
            assert_eq!(SessionId(bad.to_owned()).room(), None, "{bad:?}");
        }
        // A valid encoding of an empty room type is not routable either.
        let payload = URL_SAFE_NO_PAD.encode("/name");
        let sid = SessionId(format!("hs_{payload}_{}", "0123456789abcdef"));
        assert_eq!(sid.room(), None);
    }

    #[test]
    fn tickets_are_unique_hex_and_store_only_a_hash() {
        let (ticket, hash) = new_ticket();
        assert_eq!(ticket.len(), 32);
        assert!(ticket.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(hash_ticket(&ticket), hash);
        assert_eq!(hash.len(), 32, "SHA-256");
        let (other, other_hash) = new_ticket();
        assert_ne!(ticket, other);
        assert_ne!(hash, other_hash);
    }

    #[test]
    fn secrets_are_unique_hex_and_store_only_a_hash() {
        let (secret, hash) = new_secret();
        assert_eq!(secret.len(), 64);
        assert_eq!(hash_secret(&secret), hash);
        assert_ne!(new_secret().0, secret);
        assert!(!constant_time_eq(&hash, &hash_secret("other")));
        assert!(constant_time_eq(&hash, &hash_secret(&secret)));
    }

    #[test]
    fn reserved_events_and_defaults() {
        assert!(is_reserved_event("hearth.handoff"));
        assert!(!is_reserved_event("ready"));
        let req = HandoffRequest::new(vec![PlayerId("anon_a".to_owned())], json!({"map": "dust"}));
        assert_eq!(req.start_timeout, DEFAULT_START_TIMEOUT);
        assert_eq!(req.heartbeat_timeout, DEFAULT_HEARTBEAT_TIMEOUT);
        assert_eq!(req.max_duration, DEFAULT_MAX_DURATION);
        assert_eq!(HandoffError::Gone.status(), 410);
        assert_eq!(
            SessionStatus::parse("started"),
            Some(SessionStatus::Started)
        );
        assert!(SessionStatus::Started.is_active());
        assert!(!SessionStatus::Ended.is_active());
    }
}
