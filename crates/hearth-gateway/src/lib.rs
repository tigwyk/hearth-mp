//! Hearth Gateway Worker library: auth providers, token verification and room routing.

#![warn(missing_docs)]

pub mod auth;
pub mod providers;
pub mod ratelimit;
pub mod routes;
#[doc(hidden)]
pub mod webcrypto;

pub use auth::{
    anonymous_player_id, issue_token, mint_room_name, namespaced, sanitize_name, valid_room_name,
    verify_token, AuthError, Identity,
};
pub use providers::{
    check_enabled, AuthConfig, CannedFetcher, Fetcher, HttpReq, HttpResp, ProviderError,
    WorkerFetcher,
};
pub use routes::{forwarded_header, mm_room, serve, GatewayConfig, PlayerRoute, RoomRoute};
