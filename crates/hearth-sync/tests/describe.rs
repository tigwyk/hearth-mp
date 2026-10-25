//! Plan v0.2a Task 4: the machine-readable schema description.

use hearth_protocol::PlayerId;
use hearth_sync::{
    HearthState, Registry, Schema, Shape, SyncMap, TypeDef, TypeRef, Variant, Vis,
};
use serde::{Deserialize, Serialize};

#[derive(Schema, Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
pub struct Player {
    pub name: String,
    pub score: u32,
}

#[derive(Schema, Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum Card {
    Num(u8),
    Joker,
}

#[derive(HearthState, Serialize, Deserialize, Default)]
pub struct Match {
    #[sync]
    pub players: SyncMap<PlayerId, Player>,
    #[sync]
    pub turn: PlayerId,
    #[sync(owner)]
    pub hand: SyncMap<PlayerId, Vec<Card>>,
    pub deck: Vec<Card>,
}

fn struct_fields(def: &TypeDef) -> Vec<hearth_sync::Field> {
    match def {
        TypeDef::Struct { fields } => fields.clone(),
        TypeDef::Enum { .. } => panic!("expected a struct definition, got {def:?}"),
    }
}

#[test]
fn spec_match_describes_only_sync_fields() {
    let mut registry = Registry::default();
    let root = <Match as Schema>::describe(&mut registry);
    assert_eq!(root, TypeRef::Named("Match".to_owned()));

    let fields = struct_fields(&registry.types["Match"].1);
    let players = fields
        .iter()
        .find(|field| field.name == "players")
        .expect("players is described");
    assert_eq!(
        players.ty,
        TypeRef::SyncMap(
            Box::new(TypeRef::PlayerId),
            Box::new(TypeRef::Named("Player".to_owned()))
        )
    );
    assert_eq!(players.vis, Vis::Public);

    let turn = fields
        .iter()
        .find(|field| field.name == "turn")
        .expect("turn is described");
    assert_eq!(turn.ty, TypeRef::PlayerId);
    assert_eq!(turn.vis, Vis::Public);

    let hand = fields
        .iter()
        .find(|field| field.name == "hand")
        .expect("hand is described");
    assert_eq!(
        hand.ty,
        TypeRef::SyncMap(
            Box::new(TypeRef::PlayerId),
            Box::new(TypeRef::List(Box::new(TypeRef::Named("Card".to_owned()))))
        )
    );
    assert_eq!(hand.vis, Vis::Owner);

    assert!(
        fields.iter().all(|field| field.name != "deck"),
        "server-only fields are not described"
    );

    // Named types referenced by the state are registered too.
    assert!(registry.types.contains_key("Player"));
    assert!(registry.types.contains_key("Card"));
}

#[test]
fn enum_shapes() {
    let mut registry = Registry::default();
    let root = <Card as Schema>::describe(&mut registry);
    assert_eq!(root, TypeRef::Named("Card".to_owned()));

    let TypeDef::Enum { variants } = &registry.types["Card"].1 else {
        panic!("Card is an enum");
    };
    assert_eq!(
        variants,
        &vec![
            Variant {
                name: "Num".to_owned(),
                shape: Shape::Newtype(TypeRef::U8),
            },
            Variant {
                name: "Joker".to_owned(),
                shape: Shape::Unit,
            },
        ]
    );
}

#[test]
#[should_panic(expected = "two schema types are named `Card`")]
fn duplicate_type_name_panics() {
    mod first {
        #[allow(dead_code)]
        #[derive(hearth_sync::Schema)]
        pub struct Card {
            pub rank: u8,
        }
    }
    mod second {
        #[allow(dead_code)]
        #[derive(hearth_sync::Schema)]
        pub struct Card {
            pub label: String,
        }
    }

    let mut registry = Registry::default();
    <first::Card as Schema>::describe(&mut registry);
    <second::Card as Schema>::describe(&mut registry);
}

#[test]
fn same_type_twice_is_fine() {
    let mut registry = Registry::default();
    <Player as Schema>::describe(&mut registry);
    <Player as Schema>::describe(&mut registry);
    assert_eq!(registry.types.len(), 1);
}

#[test]
fn type_refs_serialise_the_documented_json() {
    let json = |ty: TypeRef| serde_json::to_value(ty).expect("a TypeRef serialises");
    assert_eq!(json(TypeRef::PlayerId), serde_json::json!("player_id"));
    assert_eq!(json(TypeRef::String), serde_json::json!("string"));
    assert_eq!(json(TypeRef::Unit), serde_json::json!("unit"));
    assert_eq!(
        json(TypeRef::List(Box::new(TypeRef::U8))),
        serde_json::json!({"list": "u8"})
    );
    assert_eq!(
        json(TypeRef::SyncMap(
            Box::new(TypeRef::PlayerId),
            Box::new(TypeRef::U8)
        )),
        serde_json::json!({"sync_map": ["player_id", "u8"]})
    );
    assert_eq!(
        json(TypeRef::Array(
            Box::new(TypeRef::Named("Card".to_owned())),
            3
        )),
        serde_json::json!({"array": [{"named": "Card"}, 3]})
    );
}
