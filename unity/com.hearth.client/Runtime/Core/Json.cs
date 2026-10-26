using System.Text;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>JSON and UTF-8 helpers for the plugin boundary.</summary>
    internal static class Json
    {
        /// <summary>A NUL-terminated UTF-8 string for a <c>const char*</c> argument.</summary>
        internal static byte[] Utf8Z(string s) => Encoding.UTF8.GetBytes((s ?? "") + "\0");

        internal static string Utf8(byte[] bytes) => bytes == null ? "" : Encoding.UTF8.GetString(bytes);

        internal static string Compact(JToken token) => token == null ? "null" : token.ToString(Formatting.None);

        /// <summary>Parse plugin JSON; empty input (an error result) is JSON null.</summary>
        internal static JToken Parse(string text) =>
            string.IsNullOrEmpty(text) ? JValue.CreateNull() : JToken.Parse(text);

        internal static JArray ParseArray(string text) =>
            string.IsNullOrEmpty(text) ? new JArray() : JArray.Parse(text);
    }
}
