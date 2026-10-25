//! Plan v0.2a Task 4: the exported room schema document and intent-inclusive hash.

mod support;

use hearth_server::{
    schema_doc, schema_hash_hex, Client, Ctx, JoinOpts, Leave, Mode, Result, Room, RoomConfig,
};
use hearth_sync::{HearthState, Schema};
use serde::Deserialize;
use serde_json::json;
use support::{CounterRoom, CState};

/// Same state, one extra intent variant: the only change is the intent type.
#[derive(Schema, Deserialize, Debug)]
pub enum CIntentExtra {
    Add(i64),
    Fail,
    Secret(String),
    Shout(String),
    Timer(String, u64),
    Kick(String),
    Lock,
    Dispose,
    Sql,
    Clients,
    Extra(u8),
}

#[derive(Default)]
pub struct ExtraIntentRoom;

impl RoomConfig for ExtraIntentRoom {
    const MODE: Mode = Mode::Event;
    const MAX_CLIENTS: u8 = 2;
}

impl Room for ExtraIntentRoom {
    type State = CState;
    type Intent = CIntentExtra;

    async fn on_join(
        &mut self,
        _ctx: &mut Ctx<Self>,
        _who: &Client,
        _opts: JoinOpts,
    ) -> Result<()> {
        Ok(())
    }

    async fn on_intent(
        &mut self,
        _ctx: &mut Ctx<Self>,
        _who: &Client,
        _intent: Self::Intent,
    ) -> Result<()> {
        Ok(())
    }

    async fn on_leave(
        &mut self,
        _ctx: &mut Ctx<Self>,
        _who: &Client,
        _reason: Leave,
    ) -> Result<()> {
        Ok(())
    }
}

#[test]
fn counter_room_schema_doc_export() {
    let doc = schema_doc::<CounterRoom>("CounterRoom");
    let json = serde_json::to_value(&doc).expect("SchemaDoc serialises as JSON");

    assert_eq!(json["hearth_schema"], json!(1));
    assert_eq!(json["room"], json!("CounterRoom"));
    assert_eq!(json["state"], json!("CState"));
    assert_eq!(json["intent"], json!("CIntent"));
    assert_eq!(json["hash"], json!(schema_hash_hex::<CounterRoom>()));
    assert_eq!(json["hash"].as_str().map(str::len), Some(16));

    assert_eq!(json["types"]["CState"]["kind"], json!("struct"));
    assert_eq!(json["types"]["CIntent"]["kind"], json!("enum"));
}

#[test]
fn hash_covers_the_intent_type() {
    let counter = schema_hash_hex::<CounterRoom>();
    let extra = schema_hash_hex::<ExtraIntentRoom>();

    assert_ne!(
        counter, extra,
        "changing only the intent type must change the hash"
    );
    assert_ne!(
        counter,
        <CState as HearthState>::schema_hash_hex(),
        "the room hash is not just the state hash"
    );

    // Both intents differ, and the state type is shared between the two rooms.
    let doc = schema_doc::<ExtraIntentRoom>("ExtraIntentRoom");
    assert_eq!(doc.state, "CState");
    assert_eq!(doc.intent, "CIntentExtra");
    assert_eq!(doc.hash, extra);
    assert_eq!(doc.types.len(), schema_doc::<CounterRoom>("CounterRoom").types.len());
}
