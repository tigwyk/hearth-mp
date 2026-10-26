#if UNITY_WEBGL && !UNITY_EDITOR
using System;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using UnityEngine;

namespace Hearth
{
    /// <summary>
    /// <see cref="IHearthCore"/> over the wasm core, through <c>hearth.jslib</c>.
    /// The core files are copied to <c>StreamingAssets/hearth/</c> by the
    /// editor's WebGL build hook and loaded once by <see cref="WhenReady"/>.
    /// </summary>
    public sealed class WebGLCore : IHearthCore
    {
        uint handle;
        static Task ready;

        public WebGLCore(string schemaHash, byte encoding = 0)
        {
            handle = Native.HearthCore_New(schemaHash, encoding);
            if (handle == 0)
                throw new ArgumentException(Native.HearthCore_LastError());
        }

        /// <summary>Load the wasm core (once) and wait for it.</summary>
        public static Task WhenReady() => ready ??= Load();

        static async Task Load()
        {
            Native.HearthCore_Init(Application.streamingAssetsPath + "/hearth/");
            int state;
            while ((state = Native.HearthCore_Ready()) == 0)
                await Task.Yield();
            if (state < 0)
            {
                ready = null;
                throw new HearthException("WEBGL_CORE", "could not load StreamingAssets/hearth/hearth_core.js; see the browser console");
            }
        }

        static byte[] Take(int len, out bool text)
        {
            text = Native.HearthCore_LastIsText() != 0;
            if (len <= 0)
                return null;
            var bytes = new byte[len];
            Native.HearthCore_TakeLast(bytes);
            return bytes;
        }

        public byte[] Hello(string optsJson, out bool text) => Take(Native.HearthCore_Hello(handle, optsJson ?? ""), out text);

        public JArray Handle(byte[] data, bool text) =>
            Json.ParseArray(Native.HearthCore_Handle(handle, data, data?.Length ?? 0, text ? 1 : 0));

        public uint QueueIntent(JToken intent)
        {
            var seq = Native.HearthCore_QueueIntent(handle, Json.Compact(intent));
            if (seq == 0)
                throw new ArgumentException(Native.HearthCore_LastError());
            return seq;
        }

        public byte[] PollSend(ulong nowMs, out bool text) => Take(Native.HearthCore_PollSend(handle, nowMs), out text);

        public byte[] Leave(out bool text) => Take(Native.HearthCore_Leave(handle), out text);

        public JArray OnDisconnected() => Json.ParseArray(Native.HearthCore_OnDisconnected(handle));

        public JToken State() => Json.Parse(Native.HearthCore_State(handle));

        public ulong Seq => (ulong)Native.HearthCore_Seq(handle);

        public void Dispose()
        {
            if (handle == 0)
                return;
            Native.HearthCore_Free(handle);
            handle = 0;
        }

        internal static class Native
        {
            [DllImport("__Internal")] internal static extern void HearthCore_Init(string baseUrl);
            [DllImport("__Internal")] internal static extern int HearthCore_Ready();
            [DllImport("__Internal")] internal static extern string HearthCore_LastError();
            [DllImport("__Internal")] internal static extern void HearthCore_TakeLast(byte[] buffer);
            [DllImport("__Internal")] internal static extern int HearthCore_LastIsText();
            [DllImport("__Internal")] internal static extern uint HearthCore_New(string schemaHash, int encoding);
            [DllImport("__Internal")] internal static extern void HearthCore_Free(uint h);
            [DllImport("__Internal")] internal static extern int HearthCore_Hello(uint h, string optsJson);
            [DllImport("__Internal")] internal static extern string HearthCore_Handle(uint h, byte[] data, int len, int isText);
            [DllImport("__Internal")] internal static extern uint HearthCore_QueueIntent(uint h, string intentJson);
            [DllImport("__Internal")] internal static extern int HearthCore_PollSend(uint h, double nowMs);
            [DllImport("__Internal")] internal static extern int HearthCore_Leave(uint h);
            [DllImport("__Internal")] internal static extern string HearthCore_OnDisconnected(uint h);
            [DllImport("__Internal")] internal static extern string HearthCore_State(uint h);
            [DllImport("__Internal")] internal static extern double HearthCore_Seq(uint h);
            [DllImport("__Internal")] internal static extern double HearthCore_ReconnectDecide(int code, double firstDropMs, int attempt, double nowMs);
        }
    }
}
#endif
