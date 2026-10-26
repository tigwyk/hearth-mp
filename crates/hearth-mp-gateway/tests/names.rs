//! Plan v0.3a Task 5: matchmaker-minted room names are parsed, and anything else
//! is not a matchmaker room.

use hearth_mp_gateway::mm_room;

#[test]
fn matchmaker_room_names_parse() {
    assert_eq!(mm_room("m_enam_0123456789abcdef"), Some("enam"));
    assert_eq!(mm_room("m_any_0123456789abcdef"), Some("any"));
    assert_eq!(mm_room("m_weur_fedcba9876543210"), Some("weur"));
}

#[test]
fn other_names_are_not_matchmaker_rooms() {
    // Wrong hex length, unknown region, missing prefix, extra segments, and a
    // private v0.1 room name all fall through to "a normal room".
    for name in [
        "m_enam_0123456789abcde",
        "m_enam_0123456789abcdef0",
        "m_zzz_0123456789abcdef",
        "enam_0123456789abcdef",
        "0123456789abcdef",
        "m_enam_0123456789abcdeg",
        "m_enam_0123456789abcdef/extra",
        "r1",
    ] {
        assert_eq!(mm_room(name), None, "{name} is not a matchmaker room");
    }
}

#[test]
fn identity_headers_from_clients_are_dropped() {
    // A client must not reach a room as "Admin" or claim a matchmaker pool.
    for name in [
        "X-Hearth-Display-Name",
        "x-hearth-matchmaker",
        "X-HEARTH-PLAYER-ID",
        "X-Hearth-Provider",
        "X-Hearth-Anything-New",
        "Host",
        "authorization",
    ] {
        assert!(
            !hearth_mp_gateway::forwarded_header(name),
            "{name} must be dropped"
        );
    }
    for name in [
        "Upgrade",
        "Sec-WebSocket-Key",
        "Sec-WebSocket-Protocol",
        "User-Agent",
    ] {
        assert!(
            hearth_mp_gateway::forwarded_header(name),
            "{name} must be forwarded"
        );
    }
}
