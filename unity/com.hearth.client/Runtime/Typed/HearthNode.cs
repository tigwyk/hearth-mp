using System;
using Newtonsoft.Json.Linq;
using UnityEngine.Scripting;

namespace Hearth
{
    /// <summary>
    /// A node of the typed state mirror. <c>hearth codegen --unity</c> emits
    /// subclasses with explicit load code (no reflection), so they survive IL2CPP
    /// stripping.
    /// </summary>
    public abstract class HearthNode
    {
        /// <summary>Reload from the wire value, raising events only for what changed.</summary>
        public abstract void Load(JToken raw);

        /// <summary>The child node at a wire key (a field or a map key), or null for leaves.</summary>
        public virtual HearthNode Child(string wireKey) => null;

        /// <summary>Maps reload on the way back up a routed patch, so a nested change raises their <c>OnChange</c>.</summary>
        internal virtual bool IsMap => false;
    }

    /// <summary>A generated struct type: one property per field, <c>On&lt;Field&gt;Change</c> per leaf.</summary>
    public abstract class HearthObject : HearthNode
    {
    }

    /// <summary>The schema hash of a generated root state type, read by <c>JoinAsync&lt;TState&gt;</c>.</summary>
    [Preserve]
    [AttributeUsage(AttributeTargets.Class, Inherited = false)]
    public sealed class HearthSchemaAttribute : Attribute
    {
        public string Hash { get; }

        public HearthSchemaAttribute(string hash) => Hash = hash;
    }
}
