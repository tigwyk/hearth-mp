using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>
    /// A room with a typed state mirror. <see cref="State"/> is reloaded on each
    /// <c>FULL_STATE</c> and patched per op, before <c>OnStateReset</c>/<c>OnPatched</c> fire.
    /// </summary>
    public sealed class HearthRoom<TState> : HearthRoom where TState : HearthNode, new()
    {
        public TState State { get; } = new TState();

        protected override void AfterReset() => State.Load(RawState);

        protected override void AfterPatch(JArray ops)
        {
            foreach (var op in ops)
                Route.Apply(State, RawState, (JObject)op);
        }
    }
}
