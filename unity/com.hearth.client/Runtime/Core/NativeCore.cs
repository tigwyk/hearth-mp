#if !(UNITY_WEBGL && !UNITY_EDITOR)
using System;
using System.Runtime.InteropServices;
using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>
    /// <see cref="IHearthCore"/> over the native plugin (<c>hearth_unity</c>, from
    /// <c>crates/hearth-unity</c>). Every returned buffer is copied into managed
    /// memory and freed at once.
    /// </summary>
    public sealed class NativeCore : IHearthCore
    {
        readonly HearthCoreHandle handle;

        /// <param name="encoding">0 MessagePack (binary frames), 1 JSON (text frames).</param>
        public NativeCore(string schemaHash, byte encoding = 0)
        {
            handle = Native.hearth_client_new(Json.Utf8Z(schemaHash), encoding);
            if (handle.IsInvalid)
                throw new ArgumentException(Native.LastError());
        }

        public byte[] Hello(string optsJson, out bool text)
        {
            var bytes = Native.Take(Native.hearth_client_hello(handle, Json.Utf8Z(optsJson), out var t));
            text = t != 0;
            return bytes;
        }

        public JArray Handle(byte[] data, bool text) =>
            Json.ParseArray(Json.Utf8(Native.Take(Native.hearth_client_handle(
                handle, data, (UIntPtr)(data?.Length ?? 0), (byte)(text ? 1 : 0)))));

        public uint QueueIntent(JToken intent)
        {
            var seq = Native.hearth_client_queue_intent(handle, Json.Utf8Z(Json.Compact(intent)));
            if (seq == 0)
                throw new ArgumentException(Native.LastError());
            return seq;
        }

        public byte[] PollSend(ulong nowMs, out bool text)
        {
            var bytes = Native.Take(Native.hearth_client_poll_send(handle, nowMs, out var t));
            text = t != 0;
            return bytes;
        }

        public byte[] Leave(out bool text)
        {
            var bytes = Native.Take(Native.hearth_client_leave(handle, out var t));
            text = t != 0;
            return bytes;
        }

        public JArray OnDisconnected() =>
            Json.ParseArray(Json.Utf8(Native.Take(Native.hearth_client_on_disconnected(handle))));

        public JToken State() => Json.Parse(Json.Utf8(Native.Take(Native.hearth_client_state(handle))));

        public ulong Seq => Native.hearth_client_seq(handle);

        public ulong LastTickT => Native.hearth_client_last_tick_t(handle);

        public void Dispose() => handle.Dispose();

        [StructLayout(LayoutKind.Sequential)]
        internal struct HearthBuf
        {
            public IntPtr ptr;
            public UIntPtr len;
            public UIntPtr cap;
        }

        internal static class Native
        {
#if UNITY_IOS && !UNITY_EDITOR
            const string Lib = "__Internal";
#else
            const string Lib = "hearth_unity";
#endif

            /// <summary>Copy a buffer out and free it; null for an empty buffer.</summary>
            internal static byte[] Take(HearthBuf buf)
            {
                if (buf.ptr == IntPtr.Zero)
                    return null;
                try
                {
                    var bytes = new byte[(int)buf.len];
                    Marshal.Copy(buf.ptr, bytes, 0, bytes.Length);
                    return bytes;
                }
                finally
                {
                    hearth_buf_free(buf);
                }
            }

            internal static string LastError()
            {
                var message = Json.Utf8(Take(hearth_last_error()));
                return message.Length == 0 ? "hearth_unity call failed" : message;
            }

            [DllImport(Lib)] internal static extern HearthCoreHandle hearth_client_new(byte[] schemaHash, byte encoding);
            [DllImport(Lib)] internal static extern void hearth_client_free(IntPtr c);
            [DllImport(Lib)] internal static extern HearthBuf hearth_client_hello(HearthCoreHandle c, byte[] optsJson, out byte isText);
            [DllImport(Lib)] internal static extern HearthBuf hearth_client_handle(HearthCoreHandle c, byte[] data, UIntPtr len, byte isText);
            [DllImport(Lib)] internal static extern uint hearth_client_queue_intent(HearthCoreHandle c, byte[] intentJson);
            [DllImport(Lib)] internal static extern HearthBuf hearth_client_poll_send(HearthCoreHandle c, ulong nowMs, out byte isText);
            [DllImport(Lib)] internal static extern HearthBuf hearth_client_leave(HearthCoreHandle c, out byte isText);
            [DllImport(Lib)] internal static extern HearthBuf hearth_client_on_disconnected(HearthCoreHandle c);
            [DllImport(Lib)] internal static extern HearthBuf hearth_client_state(HearthCoreHandle c);
            [DllImport(Lib)] internal static extern ulong hearth_client_seq(HearthCoreHandle c);
            [DllImport(Lib)] internal static extern ulong hearth_client_last_tick_t(HearthCoreHandle c);
            [DllImport(Lib)] internal static extern long hearth_reconnect_decide(ushort code, ulong firstDropMs, uint attempt, ulong nowMs);
            [DllImport(Lib)] internal static extern void hearth_buf_free(HearthBuf b);
            [DllImport(Lib)] internal static extern HearthBuf hearth_last_error();

            [DllImport(Lib)] internal static extern HearthInterpHandle hearth_interp_new(byte tickHz);
            [DllImport(Lib)] internal static extern void hearth_interp_free(IntPtr p);
            [DllImport(Lib)] internal static extern byte hearth_interp_track(HearthInterpHandle p, byte[] pathJson);
            [DllImport(Lib)] internal static extern byte hearth_interp_on_patch(HearthInterpHandle p, byte[] stateJson, ulong serverT, ulong localMs);
            [DllImport(Lib)] internal static extern void hearth_interp_reset(HearthInterpHandle p);
            [DllImport(Lib)] internal static extern HearthBuf hearth_interp_sample(HearthInterpHandle p, byte[] pathJson, ulong localMs);
            [DllImport(Lib)] internal static extern HearthBuf hearth_interp_sample_at(HearthInterpHandle p, byte[] pathJson, ulong renderT);

            [DllImport(Lib)] internal static extern HearthPredictorHandle hearth_predictor_new(byte[] logic, byte[] me, byte tickHz);
            [DllImport(Lib)] internal static extern void hearth_predictor_free(IntPtr p);
            [DllImport(Lib)] internal static extern byte hearth_predictor_input(HearthPredictorHandle p, uint seq, byte[] inputJson);
            [DllImport(Lib)] internal static extern byte hearth_predictor_on_authoritative(HearthPredictorHandle p, byte[] stateJson, byte[] ackedJson);
            [DllImport(Lib)] internal static extern void hearth_predictor_on_unconfirmed(HearthPredictorHandle p, byte[] seqsJson);
            [DllImport(Lib)] internal static extern HearthBuf hearth_predictor_state(HearthPredictorHandle p);
            [DllImport(Lib)] internal static extern HearthBuf hearth_predictor_names();
        }
    }
}
#endif
