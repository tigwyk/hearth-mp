//! Plan v0.3b Task 4 (server half): per-room configurable intent buckets.

mod support;

use std::time::Duration;

use hearth_protocol::{close, encode, Encoding, ErrorCode, Frame, IntentMsg};
use hearth_server::{Client, Ctx, JoinOpts, Leave, Mode, Result, Room, RoomConfig};
use hearth_test::TestRoom;
use serde_json::json;
use support::{CIntent, CState, CounterRoom};

/// A room with a tiny bucket: five intents, refilling one per second.
#[derive(Default)]
struct CappedRoom;

impl RoomConfig for CappedRoom {
    const MODE: Mode = Mode::Event;
    const MAX_CLIENTS: u8 = 2;
    const RECONNECT_GRACE: Duration = Duration::ZERO;
    const INTENT_RATE: u32 = 1;
    const INTENT_BURST: u32 = 5;
}

impl Room for CappedRoom {
    type State = CState;
    type Intent = CIntent;

    async fn on_join(&mut self, ctx: &mut Ctx<Self>, who: &Client, _opts: JoinOpts) -> Result<()> {
        ctx.state().seats.insert(who.player_id.clone(), who.seat);
        Ok(())
    }

    async fn on_intent(
        &mut self,
        ctx: &mut Ctx<Self>,
        _who: &Client,
        intent: CIntent,
    ) -> Result<()> {
        if let CIntent::Add(amount) = intent {
            ctx.state().count += amount;
        }
        Ok(())
    }

    async fn on_leave(&mut self, ctx: &mut Ctx<Self>, who: &Client, _reason: Leave) -> Result<()> {
        ctx.state().seats.remove(&who.player_id);
        Ok(())
    }
}

#[test]
fn custom_burst_closes_4005_at_burst_plus_one() {
    let mut r = TestRoom::<CappedRoom>::new();
    let a = r.join("a", json!({}));

    // Six intents in one frame: five fit the bucket, the sixth is rejected.
    r.intents(a, (0..6).map(|_| json!({"Add": 1})).collect());

    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::RATE_LIMITED),
        "the sixth intent closes the socket"
    );
    assert_eq!(
        r.server_state()["count"],
        json!(5),
        "exactly the configured burst was accepted"
    );
    let errors = support::errors(&r, a, 0);
    assert!(
        errors
            .iter()
            .any(|(code, _, _)| *code == ErrorCode::RateLimited),
        "the client saw a RateLimited error: {errors:?}"
    );
}

#[test]
fn refill_at_configured_rate() {
    let mut r = TestRoom::<CappedRoom>::new();
    let a = r.join("a", json!({}));
    r.intents(a, (0..5).map(|_| json!({"Add": 1})).collect());
    assert_eq!(r.server_state()["count"], json!(5));
    assert!(r.client(a).closed.is_none(), "the burst fit");

    r.advance(Duration::from_secs(1));
    r.intent(a, json!({"Add": 1}));
    assert_eq!(
        r.server_state()["count"],
        json!(6),
        "one token refilled after one second at rate 1/s"
    );

    r.intent(a, json!({"Add": 1}));
    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::RATE_LIMITED),
        "only one token had refilled"
    );
    assert_eq!(r.server_state()["count"], json!(6));
}

#[test]
fn defaults_unchanged() {
    assert_eq!(
        <CounterRoom as RoomConfig>::INTENT_RATE,
        60,
        "the default rate is 60 intents per second"
    );
    assert_eq!(
        <CounterRoom as RoomConfig>::INTENT_BURST,
        120,
        "the default burst is 120 intents"
    );

    // The v0.1 behaviour: the 121st intent in one frame closes with 4005.
    let mut r = TestRoom::<CounterRoom>::new();
    let a = r.join("a", json!({}));
    let intents: Vec<IntentMsg> = (1..=121u32)
        .map(|seq| IntentMsg {
            seq,
            intent: json!({"Add": 1}),
        })
        .collect();
    r.send_raw(a, encode(&Frame::Intent { intents }, Encoding::MsgPack));

    assert_eq!(
        r.client(a).closed.as_ref().map(|(code, _)| *code),
        Some(close::RATE_LIMITED)
    );
    assert_eq!(r.server_state()["count"], json!(120));
}
