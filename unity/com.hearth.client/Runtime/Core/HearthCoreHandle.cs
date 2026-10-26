#if !(UNITY_WEBGL && !UNITY_EDITOR)
using System;
using System.Runtime.InteropServices;

namespace Hearth
{
    /// <summary>
    /// The native <c>HearthClient*</c>. <see cref="SafeHandle"/> guarantees the
    /// handle is freed exactly once, even when Dispose races a finalizer.
    /// </summary>
    internal sealed class HearthCoreHandle : SafeHandle
    {
        public HearthCoreHandle() : base(IntPtr.Zero, true) { }

        public override bool IsInvalid => handle == IntPtr.Zero;

        protected override bool ReleaseHandle()
        {
            NativeCore.Native.hearth_client_free(handle);
            return true;
        }
    }
}
#endif
