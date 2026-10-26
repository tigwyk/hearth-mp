using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>
    /// A wire enum (serde external tagging): a unit variant is its bare name
    /// (<c>"Draw"</c>), any other is <c>{"Variant": payload}</c>. Generated
    /// subclasses add variant constants and typed <c>As&lt;Variant&gt;</c> accessors.
    /// </summary>
    public abstract class HearthEnum : HearthNode
    {
        /// <summary>The variant name as on the wire, for example <c>"Won"</c>.</summary>
        public string Variant { get; private set; } = "";

        /// <summary>The raw payload; null for unit variants.</summary>
        public JToken Value { get; private set; }

        public override void Load(JToken raw)
        {
            if (raw is JObject obj && obj.Count == 1)
            {
                foreach (var pair in obj)
                {
                    Variant = pair.Key;
                    Value = pair.Value;
                }
            }
            else if (raw?.Type == JTokenType.String)
            {
                Variant = (string)raw;
                Value = null;
            }
        }

        public override string ToString() => Value == null ? Variant : $"{Variant}({Value})";
    }
}
