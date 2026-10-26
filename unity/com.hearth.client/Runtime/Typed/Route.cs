using System.Collections.Generic;
using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>Applies one patch op to the typed mirror by reloading only the subtree it touched.</summary>
    public static class Route
    {
        /// <summary>
        /// The container path is <c>path</c> for <c>map_insert</c>/<c>list_splice</c>
        /// and <c>path[..^1]</c> for <c>set</c>/<c>del</c>. Walk <see cref="HearthNode.Child"/>
        /// to the deepest node and reload it from the matching raw subtree, then
        /// reload the maps passed on the way (innermost first) so a nested change
        /// raises their <c>OnChange</c>.
        /// </summary>
        public static void Apply(HearthNode root, JToken rawRoot, JObject op)
        {
            var path = op["path"] as JArray ?? new JArray();
            var kind = (string)op["op"];
            var depth = kind == "set" || kind == "del" ? path.Count - 1 : path.Count;

            var node = root;
            var raw = rawRoot;
            var maps = new List<(HearthNode node, JToken raw)>();
            for (var i = 0; i < depth; i++)
            {
                var segment = path[i];
                var child = node.Child(segment.Type == JTokenType.String ? (string)segment : segment.ToString());
                if (child == null)
                    break;
                JToken rawChild = raw switch
                {
                    JObject o => o[(string)segment],
                    JArray a when segment.Type == JTokenType.Integer && (int)segment < a.Count => a[(int)segment],
                    _ => null,
                };
                if (rawChild == null)
                    break;
                if (node.IsMap)
                    maps.Add((node, raw));
                node = child;
                raw = rawChild;
            }
            node.Load(raw);
            for (var i = maps.Count - 1; i >= 0; i--)
                maps[i].node.Load(maps[i].raw);
        }
    }
}
