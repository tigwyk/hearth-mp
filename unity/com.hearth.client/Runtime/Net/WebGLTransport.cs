#if UNITY_WEBGL && !UNITY_EDITOR
using System;
using System.Runtime.InteropServices;

namespace Hearth
{
    /// <summary>
    /// <see cref="IHearthTransport"/> over a browser <c>WebSocket</c>, through
    /// <c>hearth.jslib</c>. The browser queues events; <see cref="TryReceive"/>
    /// polls them.
    /// </summary>
    public sealed class WebGLTransport : IHearthTransport
    {
        int id;
        byte[] buffer = new byte[16 * 1024];

        public void Connect(string url) => id = HearthWS_Open(url);

        public void Send(byte[] data, bool text)
        {
            if (id != 0)
                HearthWS_Send(id, data, data.Length, text ? 1 : 0);
        }

        public void Close(ushort code)
        {
            if (id == 0)
                return;
            HearthWS_Close(id, code);
            id = 0;
        }

        public bool TryReceive(out TransportEvent ev)
        {
            ev = default;
            if (id == 0)
                return false;
            while (true)
            {
                var kind = HearthWS_Poll(id, buffer, buffer.Length);
                var len = BitConverter.ToInt32(buffer, 0);
                switch (kind)
                {
                    case 1:
                        ev = TransportEvent.Opened();
                        return true;
                    case 2:
                    case 3:
                        var data = new byte[len];
                        Buffer.BlockCopy(buffer, 4, data, 0, len);
                        ev = TransportEvent.Message(data, kind == 3);
                        return true;
                    case 4:
                        id = 0;
                        ev = TransportEvent.Closed((ushort)len, "");
                        return true;
                    case 5:
                        buffer = new byte[len];
                        continue;
                    default:
                        return false;
                }
            }
        }

        public void Dispose() => Close(1000);

        [DllImport("__Internal")] static extern int HearthWS_Open(string url);
        [DllImport("__Internal")] static extern void HearthWS_Send(int id, byte[] data, int len, int isText);
        [DllImport("__Internal")] static extern void HearthWS_Close(int id, int code);
        [DllImport("__Internal")] static extern int HearthWS_Poll(int id, byte[] buffer, int bufferLength);
    }
}
#endif
