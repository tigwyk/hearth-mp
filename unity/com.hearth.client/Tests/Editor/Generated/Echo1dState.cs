// Hand-written in the `hearth codegen --unity` format: the state of the stock
// plugin's built-in "hearth.echo1d" logic, used only by the predictor tests.
#nullable disable
using Newtonsoft.Json.Linq;

namespace Hearth.Tests.Generated
{
    public sealed partial class Echo1dState : global::Hearth.HearthObject
    {
        public long X { get; private set; }
        public event System.Action<long> OnXChange;
        JToken __X;

        public override void Load(JToken raw)
        {
            X = global::Hearth.Load.Leaf(raw, "x", ref __X, X, global::Hearth.Load.Conv.I64, v => OnXChange?.Invoke(v));
        }
    }
}
