namespace Hearth
{
    /// <summary>Stateless plugin functions.</summary>
    public static class HearthNative
    {
        /// <summary>
        /// The reconnect decision (the Rust <c>ReconnectPolicy</c>): the absolute
        /// retry clock value in milliseconds, or -1 to give up.
        /// </summary>
        public static long ReconnectDecide(ushort code, ulong firstDropMs, uint attempt, ulong nowMs)
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            return (long)WebGLCore.Native.HearthCore_ReconnectDecide(code, firstDropMs, (int)attempt, nowMs);
#else
            return NativeCore.Native.hearth_reconnect_decide(code, firstDropMs, attempt, nowMs);
#endif
        }
    }
}
