using System;
using System.Collections.Generic;
using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>
    /// A wire list (Rust <c>Vec</c>/array). <c>OnChange</c> fires only when the
    /// wire value differs; the items are then rebuilt (list indices have no
    /// stable identity in the protocol).
    /// </summary>
    public sealed class HearthList<T> : HearthNode
    {
        readonly Func<JToken, T> loadItem;
        readonly List<T> items = new List<T>();
        JToken raw;

        public event Action OnChange;

        public HearthList(Func<JToken, T> loadItem) => this.loadItem = loadItem;

        public T this[int index] => items[index];
        public int Count => items.Count;
        public IReadOnlyList<T> Items => items;

        public override void Load(JToken next)
        {
            var array = next as JArray ?? new JArray();
            if (raw != null && JToken.DeepEquals(raw, array))
                return;
            var first = raw == null;
            raw = array;
            items.Clear();
            foreach (var item in array)
                items.Add(loadItem(item));
            // The first load of an empty list is not a change.
            if (!(first && items.Count == 0))
                OnChange?.Invoke();
        }
    }
}
