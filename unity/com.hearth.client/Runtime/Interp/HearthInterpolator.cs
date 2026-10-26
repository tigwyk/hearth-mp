using System;
using System.Linq;
using System.Runtime.InteropServices;
using Newtonsoft.Json.Linq;
using UnityEngine;

namespace Hearth
{
    /// <summary>
    /// Snapshot interpolation for ticked rooms: create one with
    /// <see cref="HearthRoom.Interpolator"/>; it is fed from every ticked patch and
    /// samples two tick periods behind the server, so there is always a newer
    /// snapshot to move toward. Numbers and number arrays are lerped; anything
    /// else steps.
    /// </summary>
    /// <remarks>
    /// Path segments are strings (field or map key) or ints (list index); a
    /// <c>"*"</c> segment in a tracked path matches every key at its level. Like the
    /// Godot interpolator it assumes a 20 Hz room: the protocol does not carry
    /// <c>tick_hz</c>.
    /// </remarks>
    public sealed class HearthInterpolator : IDisposable
    {
        internal const int TickHz = 20;

        readonly HearthRoom room;
        Backend backend;

        /// <summary>The local clock in ms (the client's Stopwatch); tests inject one.</summary>
        internal Func<long> Clock;

        internal HearthInterpolator(HearthRoom room, Func<long> clock, int tickHz, object[][] paths)
        {
            this.room = room;
            Clock = clock;
            backend = new Backend(tickHz);
            foreach (var path in paths)
                if (!backend.Track(PathJson(path)))
                    throw new ArgumentException(PredictorBackend.LastError());
        }

        /// <summary>The interpolated value, or null before there is one.</summary>
        public JToken Sample(params object[] path) => backend == null ? null : Parse(backend.Sample(PathJson(path), (ulong)Math.Max(0, Clock())));

        /// <summary>The value as a <see cref="Vector2"/> (zero unless it is two numbers).</summary>
        public Vector2 SampleVector2(params object[] path) =>
            Sample(path) is JArray a && a.Count == 2 && IsNumber(a[0]) && IsNumber(a[1])
                ? new Vector2((float)a[0], (float)a[1])
                : Vector2.zero;

        /// <summary>The value as a float (0 unless it is a number).</summary>
        public float SampleFloat(params object[] path) => Sample(path) is JToken t && IsNumber(t) ? (float)t : 0f;

        /// <summary>The value at an explicit server time (fixture tests).</summary>
        internal JToken SampleAt(object[] path, ulong renderT) => Parse(backend.SampleAt(PathJson(path), renderT));

        /// <summary>Record a ticked snapshot received now (by <see cref="Clock"/>).</summary>
        internal void OnPatch(string stateJson, ulong serverT) => backend?.OnPatch(stateJson, serverT, (ulong)Math.Max(0, Clock()));

        internal void Reset() => backend?.Reset();

        /// <summary>Stop feeding and free the native interpolator. Idempotent.</summary>
        public void Dispose()
        {
            if (backend == null)
                return;
            room.RemoveInterpolator(this);
            backend.Dispose();
            backend = null;
        }

        static bool IsNumber(JToken t) => t.Type == JTokenType.Integer || t.Type == JTokenType.Float;

        static JToken Parse(string json) => string.IsNullOrEmpty(json) ? null : JToken.Parse(json);

        static string PathJson(object[] path) =>
            new JArray(path.Select(segment => segment is int i ? new JValue(i) : new JValue(segment?.ToString()))).ToString(Newtonsoft.Json.Formatting.None);

        /// <summary>The native (or wasm) interpolator.</summary>
        sealed class Backend : IDisposable
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            uint handle;

            public Backend(int tickHz) => handle = WebGLCore.Native.HearthInterp_New(tickHz);
            public bool Track(string pathJson) => WebGLCore.Native.HearthInterp_Track(handle, pathJson) != 0;
            public void OnPatch(string stateJson, ulong serverT, ulong localMs) => WebGLCore.Native.HearthInterp_OnPatch(handle, stateJson, serverT, localMs);
            public void Reset() => WebGLCore.Native.HearthInterp_Reset(handle);
            public string Sample(string pathJson, ulong localMs) => WebGLCore.Native.HearthInterp_Sample(handle, pathJson, localMs);
            public string SampleAt(string pathJson, ulong renderT) => WebGLCore.Native.HearthInterp_SampleAt(handle, pathJson, renderT);

            public void Dispose()
            {
                if (handle == 0)
                    return;
                WebGLCore.Native.HearthInterp_Free(handle);
                handle = 0;
            }
#else
            readonly HearthInterpHandle handle;

            public Backend(int tickHz) => handle = NativeCore.Native.hearth_interp_new((byte)tickHz);
            public bool Track(string pathJson) => NativeCore.Native.hearth_interp_track(handle, Json.Utf8Z(pathJson)) != 0;
            public void OnPatch(string stateJson, ulong serverT, ulong localMs) => NativeCore.Native.hearth_interp_on_patch(handle, Json.Utf8Z(stateJson), serverT, localMs);
            public void Reset() => NativeCore.Native.hearth_interp_reset(handle);
            public string Sample(string pathJson, ulong localMs) => Json.Utf8(NativeCore.Native.Take(NativeCore.Native.hearth_interp_sample(handle, Json.Utf8Z(pathJson), localMs)));
            public string SampleAt(string pathJson, ulong renderT) => Json.Utf8(NativeCore.Native.Take(NativeCore.Native.hearth_interp_sample_at(handle, Json.Utf8Z(pathJson), renderT)));
            public void Dispose() => handle.Dispose();
#endif
        }
    }

#if !(UNITY_WEBGL && !UNITY_EDITOR)
    internal sealed class HearthInterpHandle : SafeHandle
    {
        public HearthInterpHandle() : base(IntPtr.Zero, true) { }

        public override bool IsInvalid => handle == IntPtr.Zero;

        protected override bool ReleaseHandle()
        {
            NativeCore.Native.hearth_interp_free(handle);
            return true;
        }
    }
#endif
}
