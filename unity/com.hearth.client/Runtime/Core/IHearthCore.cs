using System;
using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>
    /// The sans-IO client core (the Rust <c>ClientCore</c>): handshake, patch
    /// apply, intent batching and send pacing. It performs no I/O; the caller owns
    /// the socket. <see cref="NativeCore"/> wraps the native plugin and
    /// <c>WebGLCore</c> the wasm build.
    /// </summary>
    public interface IHearthCore : IDisposable
    {
        /// <summary>The <c>HELLO</c> frame. <paramref name="optsJson"/> null or empty means <c>{}</c>.</summary>
        byte[] Hello(string optsJson, out bool text);

        /// <summary>Feed one received frame; returns its events (<c>{"type": ...}</c> objects).</summary>
        JArray Handle(byte[] data, bool text);

        /// <summary>Queue an intent and return its seq (>= 1). Throws <see cref="ArgumentException"/> on a bad intent.</summary>
        uint QueueIntent(JToken intent);

        /// <summary>The next <c>INTENT</c> frame, or null when nothing may be sent now.</summary>
        byte[] PollSend(ulong nowMs, out bool text);

        /// <summary>The <c>LEAVE</c> frame.</summary>
        byte[] Leave(out bool text);

        /// <summary>Record a socket drop; returns its events (at most one <c>intents_unconfirmed</c>).</summary>
        JArray OnDisconnected();

        /// <summary>The current state view; JSON null before the first <c>FULL_STATE</c>.</summary>
        JToken State();

        /// <summary>The last state sequence applied.</summary>
        ulong Seq { get; }
    }
}
