using System;
using System.Collections.Generic;
using Newtonsoft.Json.Linq;
using UnityEngine;

namespace Hearth
{
    /// <summary>
    /// A joined room. Every event is raised on the Unity main thread, from
    /// <see cref="HearthClient"/>'s <c>Update</c>, and none after the client is
    /// destroyed. An exception in a handler is logged and does not stop dispatch.
    /// </summary>
    public class HearthRoom
    {
        internal HearthClient Client;

        /// <summary>The wire state (sync fields only); JSON null before the first <c>FULL_STATE</c>.</summary>
        public JToken RawState { get; private set; } = JValue.CreateNull();

        /// <summary>The verified player id from <c>JOIN_OK</c>.</summary>
        public string PlayerId { get; internal set; }

        /// <summary>The seat from <c>JOIN_OK</c>; -1 before join.</summary>
        public int Seat { get; internal set; } = -1;

        public string RoomType { get; internal set; }
        public string Name { get; internal set; }

        /// <summary>The last state sequence applied.</summary>
        public ulong StateSeq => Client != null ? Client.Seq : 0;

        /// <summary>True between a recoverable drop and the next <c>JOIN_OK</c>.</summary>
        public bool Reconnecting { get; internal set; }

        public event Action OnStateReset;
        public event Action<JArray> OnPatched;
        public event Action<ulong, uint[]> OnAck;
        public event Action<string, JToken> OnEvent;
        /// <summary>An <c>ERROR</c> frame (or <c>DESYNC</c>); <c>refSeq</c> names the rejected intent.</summary>
        public event Action<string, string, uint?> OnError;
        /// <summary>A retry is scheduled. <c>attempt</c> is 0-based; <c>retryInMs</c> is from now.</summary>
        public event Action<int, int> OnReconnecting;
        /// <summary>A retry joined the room again.</summary>
        public event Action OnReconnected;
        /// <summary>Intents sent before a drop whose fate the server may or may not have decided. Never resent.</summary>
        public event Action<uint[]> OnIntentsUnconfirmed;
        /// <summary>The client gave up: a deliberate or fatal close, or the reconnect window is spent.</summary>
        public event Action<ushort, string> OnDisconnected;

        /// <summary>Queue an intent; returns its seq (>= 1). Sent on the next <c>Update</c>.</summary>
        public uint Send(JToken intent)
        {
            if (Client == null)
                throw new InvalidOperationException("the room is closed");
            return Client.QueueIntent(intent);
        }

#if UNITY_EDITOR || DEVELOPMENT_BUILD
        /// <summary>Test seam: drop the socket as an abnormal (1006) close.</summary>
        internal void DebugDrop() => Client?.DebugDrop();
#endif

#if UNITY_EDITOR
        const int PatchLogSize = 200;
        readonly Queue<(ulong seq, string opsJson)> patchLog = new Queue<(ulong, string)>();

        /// <summary>The last 200 patches, oldest first (the Hearth editor window).</summary>
        internal IReadOnlyCollection<(ulong seq, string opsJson)> PatchLog => patchLog;
#endif

        /// <summary>The typed room reloads its mirror here, before the event.</summary>
        protected virtual void AfterReset() { }

        /// <summary>The typed room routes the ops here, before the event.</summary>
        protected virtual void AfterPatch(JArray ops) { }

        internal void ApplyReset(JToken state)
        {
            RawState = state;
            Guard(AfterReset);
            Raise(OnStateReset);
        }

        internal void ApplyPatch(JToken state, JArray ops)
        {
            RawState = state;
#if UNITY_EDITOR
            patchLog.Enqueue((StateSeq, Json.Compact(ops)));
            while (patchLog.Count > PatchLogSize)
                patchLog.Dequeue();
#endif
            Guard(() => AfterPatch(ops));
            if (OnPatched != null)
                Guard(() => OnPatched(ops));
        }

        internal void RaiseAck(ulong stateSeq, uint[] seqs) { if (OnAck != null) Guard(() => OnAck(stateSeq, seqs)); }
        internal void RaiseEvent(string name, JToken data) { if (OnEvent != null) Guard(() => OnEvent(name, data)); }
        internal void RaiseError(string code, string message, uint? refSeq) { if (OnError != null) Guard(() => OnError(code, message, refSeq)); }
        internal void RaiseReconnecting(int attempt, int retryInMs) { if (OnReconnecting != null) Guard(() => OnReconnecting(attempt, retryInMs)); }
        internal void RaiseReconnected() => Raise(OnReconnected);
        internal void RaiseIntentsUnconfirmed(uint[] seqs) { if (OnIntentsUnconfirmed != null) Guard(() => OnIntentsUnconfirmed(seqs)); }
        internal void RaiseDisconnected(ushort code, string reason) { if (OnDisconnected != null) Guard(() => OnDisconnected(code, reason)); }

        static void Raise(Action handler)
        {
            if (handler != null)
                Guard(handler);
        }

        static void Guard(Action body)
        {
            try
            {
                body();
            }
            catch (Exception e)
            {
                Debug.LogException(e);
            }
        }
    }
}
