//! Hearth v0.1 Gateway Worker library: anonymous auth, token verification and room routing.

pub mod auth;
pub mod routes;

pub use auth::{
    anonymous_player_id, issue_token, mint_room_name, valid_room_name, verify_token, AuthError,
};
pub use routes::{mm_room, serve, GatewayConfig, PlayerRoute, RoomRoute};
