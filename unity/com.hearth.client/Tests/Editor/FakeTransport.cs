using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Text;

namespace Hearth.Tests
{
    /// <summary>A scripted <see cref="IHearthTransport"/>: tests push server events, then call <c>Tick</c>.</summary>
    public sealed class FakeTransport : IHearthTransport
    {
        readonly ConcurrentQueue<TransportEvent> incoming = new ConcurrentQueue<TransportEvent>();
        public readonly List<(byte[] data, bool text)> Sent = new List<(byte[], bool)>();
        public string Url;
        public ushort? ClosedWith;
        public bool Disposed;

        public void Connect(string url) => Url = url;
        public void Send(byte[] data, bool text) => Sent.Add((data, text));
        public void Close(ushort code) => ClosedWith = code;
        public bool TryReceive(out TransportEvent ev) => incoming.TryDequeue(out ev);
        public void Dispose() => Disposed = true;

        public void Push(TransportEvent ev) => incoming.Enqueue(ev);
        public void Server(string json) => Push(TransportEvent.Message(Encoding.UTF8.GetBytes(json), true));
        public string SentText(int i) => Encoding.UTF8.GetString(Sent[i].data);
    }
}
