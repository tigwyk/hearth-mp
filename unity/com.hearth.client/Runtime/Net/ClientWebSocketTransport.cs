#if !(UNITY_WEBGL && !UNITY_EDITOR)
using System;
using System.Collections.Concurrent;
using System.IO;
using System.Net.WebSockets;
using System.Threading;
using System.Threading.Tasks;

namespace Hearth
{
    /// <summary>
    /// <see cref="IHearthTransport"/> over <see cref="ClientWebSocket"/>. The receive
    /// loop runs on the thread pool and only enqueues; a failed connect is
    /// reported as <c>Closed(1006, message)</c>.
    /// </summary>
    public sealed class ClientWebSocketTransport : IHearthTransport
    {
        readonly ClientWebSocket ws = new ClientWebSocket();
        readonly CancellationTokenSource cts = new CancellationTokenSource();
        readonly ConcurrentQueue<TransportEvent> events = new ConcurrentQueue<TransportEvent>();
        readonly object sendLock = new object();
        // ClientWebSocket allows one SendAsync at a time, so sends are chained.
        Task sendChain = Task.CompletedTask;
        bool closing;

        public void Connect(string url) => _ = Run(new Uri(url));

        async Task Run(Uri uri)
        {
            try
            {
                await ws.ConnectAsync(uri, cts.Token).ConfigureAwait(false);
                events.Enqueue(TransportEvent.Opened());
                var buffer = new byte[16 * 1024];
                using var message = new MemoryStream();
                while (true)
                {
                    var result = await ws.ReceiveAsync(new ArraySegment<byte>(buffer), cts.Token).ConfigureAwait(false);
                    if (result.MessageType == WebSocketMessageType.Close)
                    {
                        events.Enqueue(TransportEvent.Closed(
                            (ushort)(result.CloseStatus ?? (WebSocketCloseStatus)1005),
                            result.CloseStatusDescription));
                        return;
                    }
                    message.Write(buffer, 0, result.Count);
                    if (!result.EndOfMessage)
                        continue;
                    events.Enqueue(TransportEvent.Message(message.ToArray(), result.MessageType == WebSocketMessageType.Text));
                    message.SetLength(0);
                }
            }
            catch (Exception e)
            {
                events.Enqueue(TransportEvent.Closed(1006, e.Message));
            }
        }

        public void Send(byte[] data, bool text) => Chain(() => ws.SendAsync(
            new ArraySegment<byte>(data),
            text ? WebSocketMessageType.Text : WebSocketMessageType.Binary,
            true,
            cts.Token));

        public void Close(ushort code)
        {
            closing = true;
            Chain(async () =>
            {
                if (ws.State == WebSocketState.Open || ws.State == WebSocketState.CloseReceived)
                {
                    using var timeout = new CancellationTokenSource(2000);
                    await ws.CloseOutputAsync((WebSocketCloseStatus)code, "", timeout.Token).ConfigureAwait(false);
                }
                cts.Cancel();
                ws.Dispose();
            });
        }

        void Chain(Func<Task> send)
        {
            lock (sendLock)
            {
                // A failed send surfaces through the receive loop as a close.
                sendChain = sendChain.ContinueWith(_ => send(), TaskScheduler.Default).Unwrap()
                    .ContinueWith(t => { _ = t.Exception; }, TaskScheduler.Default);
            }
        }

        public bool TryReceive(out TransportEvent ev) => events.TryDequeue(out ev);

        /// <summary>Abort at once, unless <see cref="Close"/> already owns the shutdown.</summary>
        public void Dispose()
        {
            if (closing)
                return;
            closing = true;
            cts.Cancel();
            ws.Abort();
            ws.Dispose();
        }
    }
}
#endif
