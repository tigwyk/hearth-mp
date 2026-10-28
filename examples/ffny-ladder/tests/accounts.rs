//! The account service the game and the relay talk to, in the native test world:
//! the sign-in handshake, a ranked ticket, the relay's redeem and pairing calls, two
//! agreeing match reports, the ratings and the leaderboard.

use ffny_ladder::accounts::{AccReply, AccRpc, Accounts};
use hearth_mp_test::world::TestWorld;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn world() -> TestWorld {
    let mut world = TestWorld::new();
    world.add_service::<Accounts>("global");
    world
}

fn call(w: &mut TestWorld, rpc: AccRpc) -> AccReply {
    w.call::<Accounts>("global", rpc).expect("accounts call")
}

fn body(r: &AccReply) -> Value {
    serde_json::from_str(&r.body).unwrap_or(Value::Null)
}

/// The game's own sign-in: a secret, `login = sha256(secret)`, then polling. Returns (token, id).
fn sign_in(w: &mut TestWorld, name: &str, secret: &str) -> (String, i64) {
    let login = hex(&Sha256::digest(secret.as_bytes()));
    assert_eq!(call(w, AccRpc::LoginPage { login: login.clone(), dev: true }).status, 200);
    assert_eq!(call(w, AccRpc::Poll { secret: secret.into() }).status, 202, "waiting before the form");
    assert_eq!(call(w, AccRpc::DevComplete { login, name: name.into(), dev: true }).status, 200);
    let polled = call(w, AccRpc::Poll { secret: secret.into() });
    assert_eq!(polled.status, 200);
    let token = body(&polled)["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 64, "the game requires a 64-digit token");
    let me = call(w, AccRpc::Me { token: token.clone() });
    (token, body(&me)["id"].as_i64().unwrap())
}

fn report(slot: i64, winner: i64, key: &str) -> String {
    json!({
        "key": key, "mode": 1, "venue": 2, "you": slot,
        "players": [{"slot": 0, "fighter": 5}, {"slot": 1, "fighter": 9}],
        "winners": [winner], "draw": false
    })
    .to_string()
}

/// Two signed-in players paired on `ABC123-1A2B3C4D`: returns their tokens.
fn paired(w: &mut TestWorld) -> (String, String) {
    let (a, ida) = sign_in(w, "Masa", &"a".repeat(64));
    let (b, idb) = sign_in(w, "Kenji", &"b".repeat(64));
    for token in [&a, &b] {
        let t = call(w, AccRpc::Ticket { token: token.clone(), text: true });
        let ticket = t.body.trim().to_owned();
        assert_eq!(ticket.len(), 32);
        let redeemed = call(w, AccRpc::Redeem { ticket: ticket.clone() });
        assert_eq!(redeemed.status, 200);
        assert_eq!(body(&redeemed)["rating"], 1500.0);
        assert_eq!(call(w, AccRpc::Redeem { ticket }).status, 404, "a ticket is used once");
    }
    let pairing = json!({"key": "ABC123-1A2B3C4D", "seats": [{"slot": 0, "account": ida}, {"slot": 1, "account": idb}]});
    assert_eq!(call(w, AccRpc::Pair { body: pairing.to_string() }).status, 204);
    (a, b)
}

#[test]
fn agreeing_reports_move_ratings_and_the_board_shows_them() {
    let mut w = world();
    let (a, b) = paired(&mut w);
    // Three games put both on the leaderboard (the game's minimum).
    for n in 0..3 {
        let key = format!("ABC123-1A2B3C4D-{n}");
        assert_eq!(call(&mut w, AccRpc::Report { token: a.clone(), body: report(0, 0, &key) }).status, 202);
        assert_eq!(call(&mut w, AccRpc::Report { token: b.clone(), body: report(1, 0, &key) }).status, 202);
    }
    let board = body(&call(&mut w, AccRpc::Board { discord: false }));
    let entries = board["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!((entries[0]["name"].as_str(), entries[0]["wins"].as_i64()), (Some("Masa"), Some(3)));
    assert_eq!(entries[1]["losses"], 3);

    let stats = call(&mut w, AccRpc::Stats { token: a, page: 0 });
    assert!(stats.body.contains("name=Masa\n") && stats.body.contains("rank=1\n"), "{}", stats.body);
    assert!(stats.body.contains("board=1|Masa|3|0|100\n"), "{}", stats.body);
    assert!(stats.body.ends_with("board_page=0\nboard_pages=1\nboard_total=2\n"), "{}", stats.body);
    let ranked = stats.body.lines().find(|l| l.starts_with("ranked=")).unwrap();
    let rating: i64 = ranked["ranked=".len()..].split('|').next().unwrap().parse().unwrap();
    assert!(rating > 1600 && ranked.ends_with("|3|3|0|0"), "{ranked}");
}

#[test]
fn disagreeing_reports_count_for_nobody() {
    let mut w = world();
    let (a, b) = paired(&mut w);
    let key = "ABC123-1A2B3C4D-0";
    call(&mut w, AccRpc::Report { token: a.clone(), body: report(0, 0, key) });
    // Both claim they won.
    call(&mut w, AccRpc::Report { token: b, body: report(1, 1, key) });
    let stats = call(&mut w, AccRpc::Stats { token: a, page: 0 });
    assert!(stats.body.contains("matches=0\n"), "{}", stats.body);
}

#[test]
fn a_report_from_the_wrong_seat_or_an_unpaired_key_counts_for_nobody() {
    let mut w = world();
    let (a, b) = paired(&mut w);
    // Slot 1's report sent by the account seated in slot 0.
    call(&mut w, AccRpc::Report { token: a.clone(), body: report(1, 1, "ABC123-1A2B3C4D-0") });
    call(&mut w, AccRpc::Report { token: b, body: report(1, 1, "ABC123-1A2B3C4D-0") });
    // A key the relay never paired.
    call(&mut w, AccRpc::Report { token: a.clone(), body: report(0, 0, "ZZZZZZ-00000000") });
    let stats = call(&mut w, AccRpc::Stats { token: a, page: 0 });
    assert!(stats.body.contains("matches=0\n"), "{}", stats.body);
}

#[test]
fn bad_tokens_and_closed_doors() {
    let mut w = world();
    assert_eq!(call(&mut w, AccRpc::Me { token: "nope".into() }).status, 401);
    assert_eq!(call(&mut w, AccRpc::Ticket { token: "nope".into(), text: true }).status, 401);
    // The dev sign-in is off unless the Worker enables it.
    let login = "c".repeat(64);
    assert_eq!(call(&mut w, AccRpc::LoginPage { login, dev: false }).status, 404);
    // The pairing needs real accounts.
    let pairing = json!({"key": "ABC123-1A2B3C4D", "seats": [{"slot": 0, "account": 7}, {"slot": 1, "account": 8}]});
    assert_eq!(call(&mut w, AccRpc::Pair { body: pairing.to_string() }).status, 400);
}

#[test]
fn logout_ends_the_session() {
    let mut w = world();
    let (a, _) = sign_in(&mut w, "Masa", &"a".repeat(64));
    call(&mut w, AccRpc::Logout { token: a.clone() });
    assert_eq!(call(&mut w, AccRpc::Me { token: a }).status, 401);
}
