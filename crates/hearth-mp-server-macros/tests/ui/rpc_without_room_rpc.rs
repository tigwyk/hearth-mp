use hearth_mp::{Client, Ctx, HearthState, JoinOpts, Leave, Result, Room, Schema};
use serde::{Deserialize, Serialize};

#[derive(HearthState, Serialize, Deserialize, Default)]
pub struct GameState {}

#[derive(Deserialize, Schema)]
pub enum Intent {
    Nothing,
}

#[derive(Default)]
pub struct NoRpc;

#[hearth_mp::room(mode = "event", max_clients = 2, binding = "NO_RPC", rpc)]
impl Room for NoRpc {
    type State = GameState;
    type Intent = Intent;

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
        intent: Intent,
    ) -> Result<()> {
        let Intent::Nothing = intent;
        Ok(())
    }

    async fn on_leave(&mut self, _ctx: &mut Ctx<Self>, _who: &Client, _reason: Leave) -> Result<()> {
        Ok(())
    }
}

fn main() {}