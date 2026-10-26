use hearth_mp_protocol::PlayerId;
use hearth_mp_sync::{HearthState, Schema, SyncMap};
use serde::{Deserialize, Serialize};
use serde_json::json;

fn pid(id: &str) -> PlayerId {
    PlayerId(id.to_owned())
}

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

#[test]
fn view_hides_server_only_and_other_owners() {
    let mut state = Match::default();
    state.players.insert(
        pid("a"),
        Player {
            name: "A".into(),
            score: 1,
        },
    );
    state.players.insert(
        pid("b"),
        Player {
            name: "B".into(),
            score: 2,
        },
    );
    state.turn = pid("a");
    state.hand.insert(pid("a"), vec![Card::Num(1)]);
    state.hand.insert(pid("b"), vec![Card::Joker]);
    state.deck = vec![Card::Num(9)];

    let view = state.view(&pid("a"));
    assert_eq!(view["hand"], json!({"a": [{"Num": 1}]}));
    assert!(view.get("deck").is_none());
    assert_eq!(view["turn"], json!("a"));
    assert_eq!(
        view["players"],
        json!({"a": {"name": "A", "score": 1}, "b": {"name": "B", "score": 2}})
    );
}

/// A state with no `#[sync(owner)]` field, for the `HAS_OWNER` flag.
#[derive(HearthState, Serialize, Deserialize, Default)]
pub struct Plain {
    #[sync]
    pub count: i64,
    pub hidden: i64,
}

fn populated_match() -> Match {
    let mut state = Match::default();
    state.players.insert(
        pid("a"),
        Player {
            name: "A".into(),
            score: 1,
        },
    );
    state.turn = pid("a");
    state.hand.insert(pid("a"), vec![Card::Num(1)]);
    state.hand.insert(pid("b"), vec![Card::Joker]);
    state.deck = vec![Card::Num(9)];
    state
}

#[test]
fn public_view_excludes_owner_and_server_fields() {
    let state = populated_match();

    let public = state.public_view();
    assert_eq!(public["turn"], json!("a"));
    assert_eq!(public["players"], json!({"a": {"name": "A", "score": 1}}));
    assert!(
        public.get("hand").is_none(),
        "an owner field is not public: {public}"
    );
    assert!(
        public.get("deck").is_none(),
        "a server-only field is never in a view: {public}"
    );
}

#[test]
fn owner_view_only_owner_fields_for_viewer() {
    let state = populated_match();

    let owner = state.owner_view(&pid("a"));
    assert_eq!(owner, json!({"hand": {"a": [{"Num": 1}]}}));
    assert!(
        owner.get("players").is_none() && owner.get("turn").is_none(),
        "public fields are not owner-scoped: {owner}"
    );
    assert!(owner.get("deck").is_none(), "{owner}");
    assert_eq!(
        state.owner_view(&pid("b")),
        json!({"hand": {"b": ["Joker"]}})
    );
    assert_eq!(
        state.owner_view(&pid("z")),
        json!({"hand": {}}),
        "a viewer with no owner entry gets an empty filtered map"
    );
}

#[test]
fn view_is_merge_of_both() {
    let state = populated_match();
    let viewer = pid("a");

    let public = state.public_view();
    let owner = state.owner_view(&viewer);
    let mut merged = public.as_object().cloned().expect("a public view object");
    merged.extend(owner.as_object().cloned().expect("an owner view object"));
    assert_eq!(state.view(&viewer), serde_json::Value::Object(merged));
}

#[test]
fn has_owner_flag_matches_the_fields() {
    const { assert!(<Match as HearthState>::HAS_OWNER) };
    const { assert!(!<Plain as HearthState>::HAS_OWNER) };
}

/// Same types and fields as [`Match`], but the server-only field has a
/// different type. Equality is checked by the test below.
mod ignores_server_only {
    use super::{Card, Player};
    use hearth_mp_protocol::PlayerId;
    use hearth_mp_sync::{HearthState, Schema, SyncMap};
    use serde::{Deserialize, Serialize};

    #[derive(HearthState, Serialize, Deserialize, Default)]
    pub struct Match {
        #[sync]
        pub players: SyncMap<PlayerId, Player>,
        #[sync]
        pub turn: PlayerId,
        #[sync(owner)]
        pub hand: SyncMap<PlayerId, Vec<Card>>,
        pub deck: Vec<u8>,
    }

    #[test]
    fn hash_ignores_server_only_fields() {
        assert_eq!(<Match as Schema>::HASH, <super::Match as Schema>::HASH);
    }
}

mod nested_type {
    use super::Card;
    use hearth_mp_protocol::PlayerId;
    use hearth_mp_sync::{HearthState, Schema, SyncMap};
    use serde::{Deserialize, Serialize};

    #[derive(Schema, Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
    pub struct Player {
        pub name: String,
        pub score: u32,
        pub level: u8,
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

    #[test]
    fn hash_changes_with_nested_type() {
        assert_ne!(<Match as Schema>::HASH, <super::Match as Schema>::HASH);
    }
}

mod plain_visibility {
    use super::{Card, Player};
    use hearth_mp_protocol::PlayerId;
    use hearth_mp_sync::{HearthState, Schema, SyncMap};
    use serde::{Deserialize, Serialize};

    #[derive(HearthState, Serialize, Deserialize, Default)]
    pub struct Match {
        #[sync]
        pub players: SyncMap<PlayerId, Player>,
        #[sync]
        pub turn: PlayerId,
        #[sync]
        pub hand: SyncMap<PlayerId, Vec<Card>>,
        pub deck: Vec<Card>,
    }

    #[test]
    fn hash_changes_with_visibility() {
        assert_ne!(<Match as Schema>::HASH, <super::Match as Schema>::HASH);
    }
}

#[test]
fn derive_schema_covers_tuple_and_unit_structs() {
    #[allow(dead_code)]
    #[derive(Schema)]
    struct Unit;

    #[allow(dead_code)]
    #[derive(Schema)]
    struct Tup(u8, String);

    assert_eq!(<Unit as Schema>::HASH, hearth_mp_sync::fnv1a64(b"Unit"));
    assert_ne!(<Tup as Schema>::HASH, <Unit as Schema>::HASH);
    assert_ne!(<Tup as Schema>::HASH, <(u8, String) as Schema>::HASH);
}

#[test]
fn derive_schema_registers_the_sync_helper_attribute() {
    // `#[derive(Schema)]` alone must accept `#[sync]` markers (normal helper
    // attribute registration), even though only HearthState consumes them.
    #[allow(dead_code)]
    #[derive(Schema)]
    struct Marked {
        #[sync]
        field: u8,
        #[sync(owner)]
        other: u8,
    }

    assert_ne!(<Marked as Schema>::HASH, <u8 as Schema>::HASH);
}

#[test]
fn ui() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/hearth_state_on_enum.rs");
    cases.compile_fail("tests/ui/owner_not_owner_scoped.rs");
    cases.compile_fail("tests/ui/sync_team.rs");
    cases.compile_fail("tests/ui/serde_rename.rs");
    cases.compile_fail("tests/ui/serde_rename_state.rs");
    cases.compile_fail("tests/ui/serde_tag.rs");
    cases.compile_fail("tests/ui/serde_flatten.rs");
    cases.pass("tests/ui/serde_default_ok.rs");
}
