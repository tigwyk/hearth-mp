using System;

namespace Hearth
{
    /// <summary>
    /// A WebSocket. Implementations only queue what happens; <see cref="HearthClient"/>
    /// drains the queue with <see cref="TryReceive"/> in <c>Update</c>, so no user
    /// code ever runs off the main thread.
    /// </summary>
    public interface IHearthTransport : IDisposable
    {
        void Connect(string url);
        void Send(byte[] data, bool text);
        /// <summary>Close after the queued sends. No event is queued for it.</summary>
        void Close(ushort code);
        bool TryReceive(out TransportEvent ev);
    }

    public enum TransportEventKind { Opened, Message, Closed }

    public readonly struct TransportEvent
    {
        public readonly TransportEventKind Kind;
        public readonly byte[] Data;
        public readonly bool Text;
        public readonly ushort Code;
        public readonly string Reason;

        TransportEvent(TransportEventKind kind, byte[] data, bool text, ushort code, string reason)
        {
            Kind = kind;
            Data = data;
            Text = text;
            Code = code;
            Reason = reason;
        }

        public static TransportEvent Opened() => new TransportEvent(TransportEventKind.Opened, null, false, 0, null);
        public static TransportEvent Message(byte[] data, bool text) => new TransportEvent(TransportEventKind.Message, data, text, 0, null);
        public static TransportEvent Closed(ushort code, string reason) => new TransportEvent(TransportEventKind.Closed, null, false, code, reason ?? "");
    }
}
