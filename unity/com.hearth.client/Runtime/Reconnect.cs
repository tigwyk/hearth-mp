namespace Hearth
{
    /// <summary>
    /// One outage's retry bookkeeping, around <see cref="HearthNative.ReconnectDecide"/>
    /// (the same policy as the Godot client and <c>conformance/vectors/reconnect.json</c>).
    /// </summary>
    internal sealed class Reconnect
    {
        long? firstDropMs;

        /// <summary>Retries already scheduled in this outage.</summary>
        public uint Attempt { get; private set; }

        /// <summary>When the next retry is due; null when not retrying.</summary>
        public long? RetryAtMs { get; private set; }

        /// <summary>The next <c>JOIN_OK</c> is a resume, so it raises <c>OnReconnected</c>.</summary>
        public bool Resuming { get; private set; }

        /// <summary>Record a drop. Returns the retry delay in ms, or -1 to give up.</summary>
        public long OnDrop(ushort code, long nowMs)
        {
            firstDropMs ??= nowMs;
            var at = HearthNative.ReconnectDecide(code, (ulong)firstDropMs.Value, Attempt, (ulong)nowMs);
            if (at < 0)
            {
                Reset();
                return -1;
            }
            RetryAtMs = at;
            Resuming = true;
            Attempt++;
            return at - nowMs;
        }

        /// <summary>True once when a scheduled retry is due.</summary>
        public bool TakeDue(long nowMs)
        {
            if (RetryAtMs == null || nowMs < RetryAtMs)
                return false;
            RetryAtMs = null;
            return true;
        }

        /// <summary>Forget the outage (joined, left, or gave up).</summary>
        public void Reset()
        {
            firstDropMs = null;
            Attempt = 0;
            RetryAtMs = null;
            Resuming = false;
        }
    }
}
