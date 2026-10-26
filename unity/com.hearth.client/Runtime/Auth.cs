using System;
using System.Text;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using UnityEngine;
using UnityEngine.Networking;

namespace Hearth
{
    /// <summary>Device identity and the gateway's HTTP API.</summary>
    internal static class Auth
    {
        internal const string DeviceIdKey = "hearth_device_id";

        /// <summary>
        /// <paramref name="systemId"/> when the platform has one, else a GUID
        /// persisted through <paramref name="load"/>/<paramref name="save"/>
        /// (WebGL reports an empty device id).
        /// </summary>
        internal static string DeviceId(string systemId, Func<string> load, Action<string> save)
        {
            if (!string.IsNullOrEmpty(systemId) && systemId != SystemInfo.unsupportedIdentifier)
                return systemId;
            var saved = load();
            if (!string.IsNullOrEmpty(saved))
                return saved;
            var generated = Guid.NewGuid().ToString("N");
            save(generated);
            return generated;
        }

        internal static string DeviceId() => DeviceId(
            SystemInfo.deviceUniqueIdentifier,
            () => PlayerPrefs.GetString(DeviceIdKey, ""),
            id =>
            {
                PlayerPrefs.SetString(DeviceIdKey, id);
                PlayerPrefs.Save();
            });

        /// <summary>A gateway call with no answer by then fails with <c>HTTP_FAILED</c> instead of hanging.</summary>
        internal static int HttpTimeoutSeconds = 15;

        /// <summary>POST JSON and parse the JSON reply. A non-2xx status throws <c>HTTP_&lt;status&gt;</c>.</summary>
        internal static async Task<JObject> PostAsync(string url, string body, string token)
        {
            using var request = new UnityWebRequest(url, UnityWebRequest.kHttpVerbPOST)
            {
                uploadHandler = new UploadHandlerRaw(Encoding.UTF8.GetBytes(body ?? "")),
                downloadHandler = new DownloadHandlerBuffer(),
                timeout = HttpTimeoutSeconds,
            };
            request.SetRequestHeader("Content-Type", "application/json");
            if (token != null)
                request.SetRequestHeader("Authorization", "Bearer " + token);
            var done = new TaskCompletionSource<bool>();
            request.SendWebRequest().completed += _ => done.TrySetResult(true);
            await done.Task;

            var text = request.downloadHandler.text;
            if (request.result == UnityWebRequest.Result.ConnectionError)
                throw new HearthException("HTTP_FAILED", request.error);
            if (request.responseCode < 200 || request.responseCode >= 300)
                throw new HearthException("HTTP_" + request.responseCode, text);
            try
            {
                return JObject.Parse(text);
            }
            catch (Exception e)
            {
                throw new HearthException("BAD_RESPONSE", $"{url} did not return JSON: {e.Message}");
            }
        }

        /// <summary><c>http(s)://host</c> → <c>ws(s)://host/rooms/{type}/{room}?token=..</c>.</summary>
        internal static string RoomUrl(string serverUrl, string roomType, string room, string token)
        {
            var b = serverUrl.TrimEnd('/');
            if (b.StartsWith("https://"))
                b = "wss://" + b.Substring(8);
            else if (b.StartsWith("http://"))
                b = "ws://" + b.Substring(7);
            return $"{b}/rooms/{Uri.EscapeDataString(roomType)}/{Uri.EscapeDataString(room)}?token={Uri.EscapeDataString(token)}";
        }
    }
}
