using System;
using System.Runtime.InteropServices;

namespace Hearth
{
    /// <summary>
    /// One native predictor (<c>hearth_predictor_*</c>), behind an interface so the
    /// same <see cref="HearthPredictor{TState}"/> runs on the native plugin and on
    /// the WebGL core. Arguments and results are JSON; <c>false</c>/empty means the
    /// predictor is poisoned and <see cref="LastError"/> says why.
    /// </summary>
    internal interface IPredictorBackend : IDisposable
    {
        bool Input(uint seq, string inputJson);
        bool OnAuthoritative(string stateJson, string ackedJson);
        void OnUnconfirmed(string seqsJson);
        string State();
    }

    internal static class PredictorBackend
    {
        /// <summary>A predictor, or null when the plugin has no logic by that name.</summary>
        internal static IPredictorBackend Create(string logic, string me, int tickHz)
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            var h = WebGLCore.Native.HearthPredictor_New(logic, me, tickHz);
            return h == 0 ? null : new WebGLPredictor(h);
#else
            var handle = NativeCore.Native.hearth_predictor_new(Json.Utf8Z(logic), Json.Utf8Z(me), (byte)tickHz);
            return handle.IsInvalid ? null : new NativePredictor(handle);
#endif
        }

        /// <summary>The registered logic names, as a JSON array.</summary>
        internal static string Names()
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            return WebGLCore.Native.HearthPredictor_Names();
#else
            return Json.Utf8(NativeCore.Native.Take(NativeCore.Native.hearth_predictor_names()));
#endif
        }

        internal static string LastError()
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            return WebGLCore.Native.HearthCore_LastError();
#else
            return NativeCore.Native.LastError();
#endif
        }
    }

#if !(UNITY_WEBGL && !UNITY_EDITOR)
    internal sealed class HearthPredictorHandle : SafeHandle
    {
        public HearthPredictorHandle() : base(IntPtr.Zero, true) { }

        public override bool IsInvalid => handle == IntPtr.Zero;

        protected override bool ReleaseHandle()
        {
            NativeCore.Native.hearth_predictor_free(handle);
            return true;
        }
    }

    internal sealed class NativePredictor : IPredictorBackend
    {
        readonly HearthPredictorHandle handle;

        internal NativePredictor(HearthPredictorHandle handle) => this.handle = handle;

        public bool Input(uint seq, string inputJson) =>
            NativeCore.Native.hearth_predictor_input(handle, seq, Json.Utf8Z(inputJson)) != 0;

        public bool OnAuthoritative(string stateJson, string ackedJson) =>
            NativeCore.Native.hearth_predictor_on_authoritative(handle, Json.Utf8Z(stateJson), Json.Utf8Z(ackedJson)) != 0;

        public void OnUnconfirmed(string seqsJson) =>
            NativeCore.Native.hearth_predictor_on_unconfirmed(handle, Json.Utf8Z(seqsJson));

        public string State() => Json.Utf8(NativeCore.Native.Take(NativeCore.Native.hearth_predictor_state(handle)));

        public void Dispose() => handle.Dispose();
    }
#else
    internal sealed class WebGLPredictor : IPredictorBackend
    {
        uint handle;

        internal WebGLPredictor(uint handle) => this.handle = handle;

        public bool Input(uint seq, string inputJson) => WebGLCore.Native.HearthPredictor_Input(handle, seq, inputJson) != 0;

        public bool OnAuthoritative(string stateJson, string ackedJson) =>
            WebGLCore.Native.HearthPredictor_OnAuthoritative(handle, stateJson, ackedJson) != 0;

        public void OnUnconfirmed(string seqsJson) => WebGLCore.Native.HearthPredictor_OnUnconfirmed(handle, seqsJson);

        public string State() => WebGLCore.Native.HearthPredictor_State(handle);

        public void Dispose()
        {
            if (handle == 0)
                return;
            WebGLCore.Native.HearthPredictor_Free(handle);
            handle = 0;
        }
    }
#endif
}
