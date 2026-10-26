using System.Collections.Generic;
using System.Linq;
using Newtonsoft.Json.Linq;
using UnityEditor;
using UnityEngine;

namespace Hearth.Editor
{
    /// <summary>
    /// <c>Window/Hearth</c>. In play mode: every <see cref="HearthClient"/> with its
    /// room, player, seat, state seq and connection status, the live state tree
    /// and the last 200 patches. Outside play mode: the local-server settings.
    /// </summary>
    internal sealed class HearthWindow : EditorWindow
    {
        Vector2 scroll;
        readonly HashSet<string> expanded = new HashSet<string> { "" };

        [MenuItem("Window/Hearth")]
        static void Open() => GetWindow<HearthWindow>("Hearth");

        // The state changes every patch; repaint while playing.
        void Update()
        {
            if (EditorApplication.isPlaying)
                Repaint();
        }

        void OnGUI()
        {
            scroll = EditorGUILayout.BeginScrollView(scroll);
            if (EditorApplication.isPlaying)
                DrawClients();
            else
                DrawSettings();
            EditorGUILayout.EndScrollView();
        }

        static void DrawSettings()
        {
            EditorGUILayout.LabelField("Local server", EditorStyles.boldLabel);
            var enabled = EditorGUILayout.Toggle("Start on Play", LocalServer.Enabled);
            if (enabled != LocalServer.Enabled)
                LocalServer.Enabled = enabled;
            using (new EditorGUI.DisabledScope(!enabled))
            {
                LocalServer.CratePath = EditorGUILayout.TextField(
                    new GUIContent("Room crate", "Folder with the room's wrangler.toml, relative to the project"),
                    LocalServer.CratePath);
                LocalServer.Port = EditorGUILayout.IntField("Port", LocalServer.Port);
            }
            EditorGUILayout.HelpBox(
                "On Play, runs `hearth-mp dev` (or `wrangler dev`) in the room crate unless the port already answers, "
                + "and points every HearthClient at it. The server keeps running between play sessions.",
                MessageType.Info);
            if (LocalServer.Started != null && !LocalServer.Started.HasExited && GUILayout.Button("Stop local server"))
                LocalServer.Stop();
        }

        void DrawClients()
        {
            var clients = FindObjectsByType<HearthClient>(FindObjectsSortMode.None);
            if (clients.Length == 0)
            {
                EditorGUILayout.HelpBox("No HearthClient in the loaded scenes.", MessageType.Info);
                return;
            }
            foreach (var client in clients)
            {
                EditorGUILayout.LabelField(client.name, EditorStyles.boldLabel);
                var room = client.Room;
                if (room == null)
                {
                    EditorGUILayout.LabelField("Status", client.Token == null ? "not logged in" : "no room");
                    continue;
                }
                EditorGUILayout.LabelField("Room", $"{room.RoomType}/{room.Name}");
                EditorGUILayout.LabelField("Player", room.PlayerId);
                EditorGUILayout.LabelField("Seat", room.Seat.ToString());
                EditorGUILayout.LabelField("State seq", room.StateSeq.ToString());
                EditorGUILayout.LabelField("Status", client.Connected ? "connected" : room.Reconnecting ? "reconnecting" : "disconnected");

                EditorGUILayout.Space();
                EditorGUILayout.LabelField("State", EditorStyles.miniBoldLabel);
                DrawToken(client.GetHashCode() + ":", "state", room.RawState, 0);

                EditorGUILayout.Space();
                EditorGUILayout.LabelField("Patches (newest first)", EditorStyles.miniBoldLabel);
                foreach (var (seq, ops) in room.PatchLog.Reverse())
                    EditorGUILayout.LabelField(seq.ToString(), ops, EditorStyles.wordWrappedMiniLabel);
                EditorGUILayout.Space();
            }
        }

        void DrawToken(string path, string label, JToken token, int depth)
        {
            if (token is JContainer container && container.Count > 0)
            {
                var open = expanded.Contains(path);
                EditorGUI.indentLevel = depth;
                var next = EditorGUILayout.Foldout(open, $"{label}  ({(token is JArray ? "list" : "map")} of {container.Count})", true);
                if (next != open)
                {
                    if (next)
                        expanded.Add(path);
                    else
                        expanded.Remove(path);
                }
                if (next)
                {
                    if (token is JObject obj)
                        foreach (var pair in obj)
                            DrawToken(path + "/" + pair.Key, pair.Key, pair.Value, depth + 1);
                    else
                        for (var i = 0; i < container.Count; i++)
                            DrawToken(path + "/" + i, i.ToString(), token[i], depth + 1);
                }
            }
            else
            {
                EditorGUI.indentLevel = depth;
                EditorGUILayout.LabelField(label, token?.ToString(Newtonsoft.Json.Formatting.None) ?? "null");
            }
            EditorGUI.indentLevel = 0;
        }
    }
}
