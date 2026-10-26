using System;
using System.Collections.Generic;
using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>
    /// A wire map (Rust <c>Map</c>/<c>SyncMap</c>), keyed by the wire string.
    /// <c>OnAdd</c> for new keys, <c>OnRemove</c> for gone keys, <c>OnChange</c> for
    /// keys whose raw value differs. Object values are reloaded in place, so a
    /// held instance stays live.
    /// </summary>
    public sealed class HearthMap<V> : HearthNode
    {
        readonly Func<JToken, V, V> loadItem;
        readonly Func<V, HearthNode> asNode;
        readonly Dictionary<string, V> values = new Dictionary<string, V>();
        readonly Dictionary<string, JToken> raws = new Dictionary<string, JToken>();

        public event Action<string, V> OnAdd;
        public event Action<string> OnRemove;
        public event Action<string, V> OnChange;

        /// <param name="loadItem">(raw, current) → value; reload <c>current</c> in place for objects.</param>
        /// <param name="asNode">The value as a node, for object values (patch routing).</param>
        public HearthMap(Func<JToken, V, V> loadItem, Func<V, HearthNode> asNode = null)
        {
            this.loadItem = loadItem;
            this.asNode = asNode;
        }

        public V this[string key] => values[key];
        public bool ContainsKey(string key) => values.ContainsKey(key);
        public bool TryGetValue(string key, out V value) => values.TryGetValue(key, out value);
        public IReadOnlyCollection<string> Keys => values.Keys;
        public int Count => values.Count;

        internal override bool IsMap => true;

        public override HearthNode Child(string wireKey) =>
            asNode != null && values.TryGetValue(wireKey, out var value) ? asNode(value) : null;

        public override void Load(JToken raw)
        {
            var obj = raw as JObject ?? new JObject();
            List<string> removed = null;
            foreach (var key in values.Keys)
                if (!obj.ContainsKey(key))
                    (removed ??= new List<string>()).Add(key);
            if (removed != null)
                foreach (var key in removed)
                {
                    values.Remove(key);
                    raws.Remove(key);
                    OnRemove?.Invoke(key);
                }

            foreach (var pair in obj)
            {
                if (values.TryGetValue(pair.Key, out var current))
                {
                    if (JToken.DeepEquals(raws[pair.Key], pair.Value))
                        continue;
                    values[pair.Key] = loadItem(pair.Value, current);
                    raws[pair.Key] = pair.Value;
                    OnChange?.Invoke(pair.Key, values[pair.Key]);
                }
                else
                {
                    var value = loadItem(pair.Value, default);
                    values[pair.Key] = value;
                    raws[pair.Key] = pair.Value;
                    OnAdd?.Invoke(pair.Key, value);
                }
            }
        }
    }
}
