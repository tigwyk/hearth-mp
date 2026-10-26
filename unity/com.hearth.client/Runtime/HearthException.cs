using System;

namespace Hearth
{
    /// <summary>A Hearth failure with its wire code (<c>ROOM_FULL</c>, <c>HTTP_401</c>, ...).</summary>
    public class HearthException : Exception
    {
        public string Code { get; }

        public HearthException(string code, string message) : base($"{code}: {message}")
        {
            Code = code;
        }
    }
}
