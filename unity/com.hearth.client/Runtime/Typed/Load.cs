using System;
using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>Helpers the generated <c>Load</c> methods call.</summary>
    public static class Load
    {
        /// <summary>
        /// Load one leaf field (primitive, enum or option). <paramref name="last"/>
        /// caches the previous raw value; <paramref name="onChange"/> runs and the
        /// cache updates only when the raw value differs.
        /// </summary>
        public static T Leaf<T>(JToken raw, string wire, ref JToken last, T current, Func<JToken, T> conv, Action<T> onChange)
        {
            var next = raw?[wire];
            if (Same(last, next))
                return current;
            last = next;
            var value = conv(next);
            onChange?.Invoke(value);
            return value;
        }

        /// <summary>Reload an object field in place; its own fields raise their events.</summary>
        public static T Object<T>(JToken raw, string wire, T node) where T : HearthNode
        {
            node.Load(raw?[wire]);
            return node;
        }

        /// <summary>A <see cref="HearthMap{V}"/> item loader for values that are rebuilt, not reloaded in place.</summary>
        public static Func<JToken, V, V> Item<V>(Func<JToken, V> conv) => (t, _) => conv(t);

        /// <summary>A missing field and JSON null are the same value.</summary>
        static bool Same(JToken a, JToken b)
        {
            var aNull = a == null || a.Type == JTokenType.Null;
            var bNull = b == null || b.Type == JTokenType.Null;
            return aNull || bNull ? aNull && bNull : JToken.DeepEquals(a, b);
        }

        /// <summary>
        /// Explicit converters. They use the explicit <see cref="JToken"/> casts,
        /// never <c>ToObject&lt;T&gt;</c>, which breaks under AOT. A missing or
        /// null token is the default value.
        /// </summary>
        public static class Conv
        {
            static bool None(JToken t) => t == null || t.Type == JTokenType.Null;

            public static byte U8(JToken t) => None(t) ? (byte)0 : (byte)t;
            public static sbyte I8(JToken t) => None(t) ? (sbyte)0 : (sbyte)t;
            public static ushort U16(JToken t) => None(t) ? (ushort)0 : (ushort)t;
            public static short I16(JToken t) => None(t) ? (short)0 : (short)t;
            public static uint U32(JToken t) => None(t) ? 0u : (uint)t;
            public static int I32(JToken t) => None(t) ? 0 : (int)t;
            public static ulong U64(JToken t) => None(t) ? 0ul : (ulong)t;
            public static long I64(JToken t) => None(t) ? 0L : (long)t;
            public static float F32(JToken t) => None(t) ? 0f : (float)t;
            public static double F64(JToken t) => None(t) ? 0d : (double)t;
            public static bool Bool(JToken t) => !None(t) && (bool)t;
            public static string Str(JToken t) => None(t) ? null : (string)t;
            public static JArray Tuple(JToken t) => t as JArray ?? new JArray();

            /// <summary><c>Option&lt;T&gt;</c> of a value type.</summary>
            public static Func<JToken, T?> Nullable<T>(Func<JToken, T> inner) where T : struct =>
                t => None(t) ? (T?)null : inner(t);

            /// <summary><c>Option&lt;T&gt;</c> of a reference type.</summary>
            public static Func<JToken, T> Option<T>(Func<JToken, T> inner) where T : class =>
                t => None(t) ? null : inner(t);

            /// <summary>A fresh generated node loaded from <paramref name="t"/> (enum or struct values).</summary>
            public static T Node<T>(JToken t) where T : HearthNode, new()
            {
                var node = new T();
                node.Load(t);
                return node;
            }
        }
    }
}
