using System;
using System.Collections;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;
using UnityEngine;
using UnityEngine.TestTools;

namespace Hearth.Tests
{
    /// <summary>
    /// Two clients play tic-tac-toe against a real gateway (<c>wrangler dev</c> in
    /// <c>examples/tictactoe</c>). Set <c>HEARTH_URL</c> and <c>HEARTH_SCHEMA</c>;
    /// otherwise the test is ignored.
    /// </summary>
    public class ClientPlayTests
    {
        static readonly string Url = Environment.GetEnvironmentVariable("HEARTH_URL");
        static readonly string Schema = Environment.GetEnvironmentVariable("HEARTH_SCHEMA");

        static IEnumerator Await(Task task, float timeout = 30f)
        {
            var deadline = Time.realtimeSinceStartup + timeout;
            while (!task.IsCompleted)
            {
                if (Time.realtimeSinceStartup > deadline)
                    Assert.Fail("timed out");
                yield return null;
            }
            if (task.IsFaulted)
                throw task.Exception.InnerException;
        }

        static IEnumerator Until(Func<bool> done, float timeout = 30f)
        {
            var deadline = Time.realtimeSinceStartup + timeout;
            while (!done())
            {
                if (Time.realtimeSinceStartup > deadline)
                    Assert.Fail("timed out");
                yield return null;
            }
        }

        static HearthClient NewClient(string device)
        {
            var client = new GameObject(device).AddComponent<HearthClient>();
            client.ServerUrl = Url;
            client.SchemaHash = Schema;
            client.DeviceIdOverride = device + "-" + Guid.NewGuid().ToString("N");
            return client;
        }

        static JToken Place(int cell) => JToken.Parse("{\"Place\":{\"cell\":" + cell + "}}");

        [UnityTest, Timeout(30000)]
        public IEnumerator TwoClients_PlayAGame_ThenResumeAfterDrop()
        {
            if (string.IsNullOrEmpty(Url) || string.IsNullOrEmpty(Schema))
                Assert.Ignore("set HEARTH_URL and HEARTH_SCHEMA to run against wrangler dev");

            var a = NewClient("a");
            var b = NewClient("b");
            try
            {
                yield return Await(a.LoginAnonymousAsync());
                yield return Await(b.LoginAnonymousAsync());
                var create = a.CreateRoomAsync("tictactoe");
                yield return Await(create);
                var joinA = a.JoinAsync("tictactoe", create.Result);
                yield return Await(joinA);
                var joinB = b.JoinAsync("tictactoe", create.Result);
                yield return Await(joinB);
                var roomA = joinA.Result;
                var roomB = joinB.Result;
                yield return Until(() => roomA.RawState["status"]?.Type == JTokenType.String && (string)roomA.RawState["status"] == "Playing");

                var x = (string)roomA.RawState["turn"] == roomA.PlayerId ? roomA : roomB;
                var o = x == roomA ? roomB : roomA;
                foreach (var (room, cell) in new[] { (x, 0), (o, 3), (x, 1), (o, 4), (x, 2) })
                {
                    var before = (int)room.RawState["cells"][cell];
                    room.Send(Place(cell));
                    yield return Until(() => (int)room.RawState["cells"][cell] != before);
                }
                var won = JToken.Parse("{\"Won\":\"" + x.PlayerId + "\"}");
                yield return Until(() => JToken.DeepEquals(roomA.RawState["status"], won) && JToken.DeepEquals(roomB.RawState["status"], won));

                var seat = x.Seat;
                var reconnected = false;
                x.OnReconnected += () => reconnected = true;
                x.DebugDrop();
                x.Send("Rematch");
                yield return Until(() => reconnected);
                yield return Until(() => roomA.RawState["status"]?.Type == JTokenType.String && (string)roomA.RawState["status"] == "Playing");
                Assert.AreEqual(seat, x.Seat);
            }
            finally
            {
                UnityEngine.Object.Destroy(a.gameObject);
                UnityEngine.Object.Destroy(b.gameObject);
            }
        }
    }
}
