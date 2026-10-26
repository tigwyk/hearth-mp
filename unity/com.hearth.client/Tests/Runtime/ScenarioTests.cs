using System;
using System.Collections;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;
using UnityEngine;
using UnityEngine.TestTools;

namespace Hearth.Tests
{
    /// <summary>
    /// The conformance scenarios (<c>conformance/scenarios/*.json</c>) through
    /// <see cref="HearthClient"/>, against the reference server:
    /// <c>cargo run -p hearth-mp-conformance-server</c>, then set
    /// <c>HEARTH_CONFORMANCE_URL=http://127.0.0.1:8790</c>. Ignored when it is unset,
    /// unless <c>HEARTH_REQUIRE_CONFORMANCE=1</c>, which makes that a failure.
    /// Observed frames and pattern matching are exactly those of the Rust runner
    /// (<c>crates/hearth-mp-conformance-server/tests/scenarios.rs</c>).
    /// </summary>
    public class ScenarioTests
    {
        const float ExpectTimeout = 5f;

        sealed class Client
        {
            public HearthClient Hearth;
            public HearthRoom Room;
            public readonly List<JToken> Observed = new List<JToken>();
            public int Cursor;
            public Task Pending;
        }

        static string Url => Environment.GetEnvironmentVariable("HEARTH_CONFORMANCE_URL");

        static string ScenarioDir()
        {
            for (var dir = new DirectoryInfo(Application.dataPath); dir != null; dir = dir.Parent)
            {
                var candidate = Path.Combine(dir.FullName, "conformance", "scenarios");
                if (Directory.Exists(candidate))
                    return candidate;
            }
            Assert.Fail("conformance/scenarios not found above " + Application.dataPath);
            return null;
        }

        [UnityTest, Timeout(180000)]
        public IEnumerator EveryScenarioPasses()
        {
            if (string.IsNullOrEmpty(Url))
            {
                if (Environment.GetEnvironmentVariable("HEARTH_REQUIRE_CONFORMANCE") == "1")
                    Assert.Fail("HEARTH_CONFORMANCE_URL is not set and HEARTH_REQUIRE_CONFORMANCE=1");
                Assert.Ignore("set HEARTH_CONFORMANCE_URL to the reference server");
            }
            var files = Directory.GetFiles(ScenarioDir(), "*.json").OrderBy(f => f, StringComparer.Ordinal).ToArray();
            Assert.IsNotEmpty(files, "no scenarios");
            var schema = new SchemaFetch();
            yield return schema.Run(Url);
            Assert.IsNotEmpty(schema.Hash, "GET /schema/conf failed");
            foreach (var file in files)
            {
                yield return RunScenario(JObject.Parse(File.ReadAllText(file)), schema.Hash);
                Debug.Log("ok - " + Path.GetFileName(file));
            }
        }

        sealed class SchemaFetch
        {
            public string Hash;

            public IEnumerator Run(string url)
            {
                using var request = UnityEngine.Networking.UnityWebRequest.Get(url.TrimEnd('/') + "/schema/conf");
                yield return request.SendWebRequest();
                if (request.result == UnityEngine.Networking.UnityWebRequest.Result.Success)
                    Hash = (string)JObject.Parse(request.downloadHandler.text)["schema_hash"];
            }
        }

        IEnumerator RunScenario(JObject scenario, string schema)
        {
            var name = (string)scenario["name"];
            var room = $"{name}-{DateTime.UtcNow.Ticks}";
            var clients = new Dictionary<string, Client>();
            try
            {
                foreach (var id in scenario["clients"])
                {
                    var client = new Client { Hearth = new GameObject("scenario-" + id).AddComponent<HearthClient>() };
                    client.Hearth.ServerUrl = Url;
                    client.Hearth.SchemaHash = schema;
                    client.Hearth.DeviceIdOverride = (string)id;
                    clients[(string)id] = client;
                    yield return Await(client.Hearth.LoginAnonymousAsync());
                }
                foreach (var step in scenario["steps"])
                {
                    if (step["wait_ms"] != null)
                    {
                        yield return new WaitForSecondsRealtime((float)step["wait_ms"] / 1000f);
                        continue;
                    }
                    var client = clients[(string)step["client"]];
                    if (step["expect"] != null)
                    {
                        yield return Expect(client, step["expect"], name, (string)step["client"]);
                        continue;
                    }
                    var send = step["send"];
                    if (send.Type == JTokenType.String)
                    {
                        if ((string)send == "connect")
                            Join(client, room, null);
                        else if ((string)send == "drop")
                            client.Room.DebugDrop(); // the SDK reconnects by itself
                        continue;
                    }
                    switch ((string)send["op"])
                    {
                        case "HELLO":
                            client.Hearth.SchemaHash = (string)send["schema_hash"];
                            Join(client, room, send["opts"] as JObject);
                            break;
                        case "INTENT":
                            foreach (var message in send["intents"])
                                client.Room.Send(message["intent"]);
                            break;
                        case "LEAVE":
                            client.Hearth.Leave();
                            break;
                    }
                }
            }
            finally
            {
                foreach (var client in clients.Values)
                    UnityEngine.Object.Destroy(client.Hearth.gameObject);
            }
            yield return null;
        }

        /// Start a join; the frames its SDK hides (JOIN_OK, FULL_STATE) are observed when it completes.
        static void Join(Client client, string room, JObject opts)
        {
            client.Pending = client.Hearth.JoinAsync("conf", room, opts).ContinueWith(task =>
            {
                if (task.IsFaulted && task.Exception.InnerException is HearthException e)
                {
                    client.Observed.Add(new JObject { ["op"] = "ERROR", ["code"] = e.Code, ["message"] = e.Message, ["ref_seq"] = null });
                    return;
                }
                if (!task.IsCompletedSuccessfully)
                    return;
                var r = task.Result;
                client.Room = r;
                client.Observed.Add(new JObject { ["op"] = "JOIN_OK", ["player_id"] = r.PlayerId, ["seat"] = r.Seat });
                client.Observed.Add(new JObject { ["op"] = "FULL_STATE", ["state"] = r.RawState.DeepClone() });
                r.OnReconnected += () => client.Observed.Add(new JObject { ["op"] = "JOIN_OK", ["player_id"] = r.PlayerId, ["seat"] = r.Seat });
                r.OnStateReset += () => client.Observed.Add(new JObject { ["op"] = "FULL_STATE", ["state"] = r.RawState.DeepClone() });
                r.OnPatched += ops => client.Observed.Add(new JObject { ["op"] = "PATCH", ["ops"] = ops, ["state"] = r.RawState.DeepClone() });
                r.OnAck += (_, seqs) => client.Observed.Add(new JObject { ["op"] = "ACK", ["intent_seqs"] = new JArray(seqs.Cast<object>().ToArray()) });
                r.OnEvent += (eventName, data) => client.Observed.Add(new JObject { ["op"] = "EVENT", ["name"] = eventName, ["data"] = data });
                r.OnError += (code, message, refSeq) => client.Observed.Add(new JObject
                {
                    ["op"] = "ERROR", ["code"] = code, ["message"] = message, ["ref_seq"] = refSeq.HasValue ? (JToken)refSeq.Value : JValue.CreateNull(),
                });
                r.OnDisconnected += (code, _) => client.Observed.Add(new JObject { ["op"] = "CLOSE", ["code"] = code });
            }, TaskScheduler.FromCurrentSynchronizationContext());
        }

        static IEnumerator Expect(Client client, JToken pattern, string scenario, string who)
        {
            var deadline = Time.realtimeSinceStartup + ExpectTimeout;
            while (true)
            {
                for (var i = client.Cursor; i < client.Observed.Count; i++)
                {
                    if (Matches(pattern, client.Observed[i]))
                    {
                        client.Cursor = i + 1;
                        yield break;
                    }
                }
                if (Time.realtimeSinceStartup > deadline)
                    Assert.Fail($"{scenario}: client {who} never observed {pattern.ToString(Newtonsoft.Json.Formatting.None)}\n"
                        + $"observed after cursor: {new JArray(client.Observed.Skip(client.Cursor)).ToString(Newtonsoft.Json.Formatting.None)}");
                yield return null;
            }
        }

        static bool Matches(JToken pattern, JToken value)
        {
            if (pattern.Type == JTokenType.String && (string)pattern == "*")
                return true;
            if (pattern is JObject p)
            {
                if (!(value is JObject v))
                    return false;
                foreach (var pair in p)
                {
                    var absent = pair.Value.Type == JTokenType.String && (string)pair.Value == "$absent";
                    if (!v.TryGetValue(pair.Key, out var actual))
                    {
                        if (!absent)
                            return false;
                        continue;
                    }
                    if (absent || !Matches(pair.Value, actual))
                        return false;
                }
                return true;
            }
            if (pattern is JArray pa)
                return value is JArray va && pa.Count == va.Count && pa.Zip(va, Matches).All(ok => ok);
            var numbers = new[] { JTokenType.Integer, JTokenType.Float };
            if (numbers.Contains(pattern.Type) && numbers.Contains(value.Type))
                return (double)pattern == (double)value;
            return JToken.DeepEquals(pattern, value);
        }

        static IEnumerator Await(Task task)
        {
            var deadline = Time.realtimeSinceStartup + 10f;
            while (!task.IsCompleted)
            {
                if (Time.realtimeSinceStartup > deadline)
                    Assert.Fail("timed out");
                yield return null;
            }
            if (task.IsFaulted)
                throw task.Exception.InnerException;
        }
    }
}
