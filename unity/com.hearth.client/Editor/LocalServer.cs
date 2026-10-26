using System;
using System.Diagnostics;
using System.IO;
using System.Net.Http;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using UnityEditor;
using Debug = UnityEngine.Debug;

namespace Hearth.Editor
{
    /// <summary>
    /// The "local server" play-mode toggle: when play mode starts, start
    /// <c>hearth-mp dev</c> (or <c>wrangler dev</c>) in the room crate unless the
    /// port already answers, and point every <see cref="HearthClient"/> at it.
    /// The server keeps running between play sessions; only a process this
    /// class started is ever killed.
    /// </summary>
    [InitializeOnLoad]
    internal static class LocalServer
    {
        const string EnabledKey = "Hearth.LocalServer.Enabled";
        const string CratePathKey = "Hearth.LocalServer.CratePath";
        const string PortKey = "Hearth.LocalServer.Port";
        const int StartTimeoutMs = 60_000;

        static readonly HttpClient Http = new HttpClient { Timeout = TimeSpan.FromSeconds(2) };

        /// <summary>The process this class started; null when the server is someone else's.</summary>
        internal static Process Started;

        static LocalServer()
        {
            EditorApplication.playModeStateChanged += OnPlayModeChanged;
            EditorApplication.quitting += Stop;
        }

        internal static bool Enabled
        {
            get => EditorPrefs.GetBool(EnabledKey, false);
            set
            {
                EditorPrefs.SetBool(EnabledKey, value);
                if (!value)
                    Stop();
            }
        }

        internal static string CratePath
        {
            get => EditorPrefs.GetString(CratePathKey, "");
            set => EditorPrefs.SetString(CratePathKey, value);
        }

        internal static int Port
        {
            get => EditorPrefs.GetInt(PortKey, 8787);
            set => EditorPrefs.SetInt(PortKey, value);
        }

        internal static string Url => $"http://127.0.0.1:{Port}";

        /// <summary>The shell command that runs the dev server.</summary>
        internal static string BuildCommand(string cratePath, int port, bool hearthOnPath) =>
            hearthOnPath ? $"hearth-mp dev --port {port}" : $"wrangler dev --port {port}";

        /// <summary>Whether a <c>GET /schema/..</c> body came from a Hearth gateway (JSON, not an HTML error page).</summary>
        internal static bool IsReady(string schemaResponseBody)
        {
            if (string.IsNullOrWhiteSpace(schemaResponseBody))
                return false;
            try
            {
                return JToken.Parse(schemaResponseBody) is JObject;
            }
            catch (Exception)
            {
                return false;
            }
        }

        internal static void OnPlayModeChanged(PlayModeStateChange change)
        {
            if (change == PlayModeStateChange.ExitingPlayMode)
            {
                HearthClient.UrlOverride = null;
            }
            else if (change == PlayModeStateChange.ExitingEditMode && Enabled)
            {
                // Set before any scene script runs, so a login in Awake/Start already uses it.
                HearthClient.UrlOverride = Url;
                _ = EnsureRunning();
            }
        }

        static async Task EnsureRunning()
        {
            if (!await Answers())
            {
                var hearthOnPath = OnPath("hearth-mp");
                var command = BuildCommand(CratePath, Port, hearthOnPath);
                if (Started == null || Started.HasExited)
                    Started = Launch(command, Path.GetFullPath(CratePath));
                var deadline = DateTime.UtcNow.AddMilliseconds(StartTimeoutMs);
                while (!await Answers())
                {
                    if (DateTime.UtcNow > deadline || Started == null || Started.HasExited)
                    {
                        Debug.LogError($"Hearth: `{command}` in {CratePath} did not answer on {Url} within 60 s; stopping play mode");
                        EditorApplication.isPlaying = false;
                        return;
                    }
                    await Task.Delay(500);
                }
            }
        }

        static async Task<bool> Answers()
        {
            try
            {
                var body = await Http.GetStringAsync($"{Url}/schema/_hearth_probe");
                return IsReady(body);
            }
            catch (HttpRequestException e) when (e.Message.Contains("404"))
            {
                // An unknown room type is still a gateway answering.
                return true;
            }
            catch (Exception)
            {
                return false;
            }
        }

        static Process Launch(string command, string workingDirectory)
        {
            var windows = Environment.OSVersion.Platform == PlatformID.Win32NT;
            var info = new ProcessStartInfo
            {
                FileName = windows ? "cmd.exe" : "/bin/sh",
                Arguments = windows ? $"/c {command}" : $"-c \"exec {command}\"",
                WorkingDirectory = workingDirectory,
                UseShellExecute = false,
                CreateNoWindow = true,
            };
            Debug.Log($"Hearth: starting `{command}` in {workingDirectory}");
            return Process.Start(info);
        }

        static bool OnPath(string tool)
        {
            var path = Environment.GetEnvironmentVariable("PATH") ?? "";
            foreach (var dir in path.Split(Path.PathSeparator))
            {
                if (File.Exists(Path.Combine(dir, tool)) || File.Exists(Path.Combine(dir, tool + ".exe")))
                    return true;
            }
            return false;
        }

        /// <summary>Kill the server, but only one this class started.</summary>
        internal static void Stop()
        {
            if (Started == null)
                return;
            try
            {
                if (!Started.HasExited)
                    KillTree(Started);
            }
            catch (Exception e)
            {
                Debug.LogWarning($"Hearth: could not stop the local server: {e.Message}");
            }
            Started = null;
        }

        static void KillTree(Process process)
        {
            // cmd/sh spawns wrangler (node) as a child; kill the whole tree.
            if (Environment.OSVersion.Platform == PlatformID.Win32NT)
                Process.Start(new ProcessStartInfo("taskkill", $"/T /F /PID {process.Id}") { CreateNoWindow = true, UseShellExecute = false })?.WaitForExit(5000);
            else
                process.Kill();
        }
    }
}
