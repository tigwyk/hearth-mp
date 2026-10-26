using System.IO;
using System.Linq;
using Newtonsoft.Json.Linq;
using NUnit.Framework;
using UnityEngine;

namespace Hearth.Tests
{
    /// <summary><see cref="HearthInterpolator"/> against the shared <c>conformance/vectors/interp.json</c> and a scripted ticked room.</summary>
    public class InterpTests
    {
        ClientLogicTests c;

        [SetUp]
        public void SetUp()
        {
            c = new ClientLogicTests();
            c.SetUp();
        }

        [TearDown]
        public void TearDown() => c.TearDown();

        static object[] Path(JToken path) =>
            path.Select(s => s.Type == JTokenType.Integer ? (object)(int)s : (string)s).ToArray();

        static bool Close(JToken a, JToken b)
        {
            if (a is JArray x && b is JArray y)
                return x.Count == y.Count && x.Zip(y, Close).All(ok => ok);
            if ((a.Type == JTokenType.Integer || a.Type == JTokenType.Float) && (b.Type == JTokenType.Integer || b.Type == JTokenType.Float))
                return System.Math.Abs((double)a - (double)b) < 1e-6;
            return JToken.DeepEquals(a, b);
        }

        [Test]
        public void Fixture_CasesMatch()
        {
            var room = c.Join();
            foreach (var @case in Vectors.Load("interp.json"))
            {
                var track = @case["track"] ?? new JArray("value");
                var sample = @case["sample"] ?? track;
                using var interp = room.Interpolator((int)@case["tick_hz"], Path(track));
                foreach (var pair in (JArray)@case["snapshots"])
                {
                    var t = (ulong)pair[0];
                    interp.Clock = () => (long)t;
                    interp.OnPatch(new JObject { ["value"] = pair[1] }.ToString(), t);
                }
                var actual = interp.SampleAt(Path(sample), (ulong)@case["render_t"]);
                var expect = @case["expect"];
                if (expect.Type == JTokenType.Null)
                    Assert.IsNull(actual, (string)@case["name"]);
                else
                    Assert.IsTrue(actual != null && Close(actual, expect), $"{@case["name"]}: {actual} vs {expect}");
            }
        }

        [Test]
        public void Wildcard_TracksNewKeys_FedFromTickedPatches()
        {
            var room = c.Join("{\"avatars\":{}}");
            var now = 0L;
            var interp = room.Interpolator(new object[] { "avatars", "*", "pos" });
            interp.Clock = () => now;

            void Tick(int seq, long t, string avatars)
            {
                now = t - 10_000; // the server clock runs 10 s ahead
                c.Socket.Server("{\"op\":\"PATCH\",\"seq\":1,\"tick\":" + seq + ",\"t\":" + t
                    + ",\"ops\":[{\"op\":\"set\",\"path\":[\"avatars\"],\"value\":" + avatars + "}]}");
                c.client.Tick();
            }

            Tick(1, 11_000, "{\"a\":{\"pos\":[0,0]}}");
            Tick(2, 11_050, "{\"a\":{\"pos\":[1,0]},\"b\":{\"pos\":[5,5]}}");
            Tick(3, 11_100, "{\"a\":{\"pos\":[2,0]},\"b\":{\"pos\":[6,5]}}");
            now = 1_175; // server 11_175, render 11_075: halfway between ticks 2 and 3
            Assert.AreEqual(new Vector2(1.5f, 0), interp.SampleVector2("avatars", "a", "pos"));
            Assert.AreEqual(new Vector2(5.5f, 5), interp.SampleVector2("avatars", "b", "pos"), "a key added later is tracked");
            Assert.AreEqual(0f, interp.SampleFloat("avatars", "a", "pos"), "not a number");
            Assert.IsNull(interp.Sample("avatars", "zz", "pos"));

            c.Socket.Server("{\"op\":\"FULL_STATE\",\"seq\":2,\"state\":{\"avatars\":{}}}");
            c.client.Tick();
            Assert.IsNull(interp.Sample("avatars", "a", "pos"), "a FULL_STATE drops the snapshots");
        }

        [Test]
        public void EventRoomPatches_DoNotFeed()
        {
            var room = c.Join("{\"x\":0}");
            var interp = room.Interpolator(new object[] { "x" });
            c.Socket.Server("{\"op\":\"PATCH\",\"seq\":2,\"ops\":[{\"op\":\"set\",\"path\":[\"x\"],\"value\":1}]}");
            c.client.Tick();
            Assert.IsNull(interp.SampleAt(new object[] { "x" }, 0));
        }

        [Test]
        public void Dispose_Twice_Safe_AndClosedRoomFreesIt()
        {
            var room = c.Join();
            var interp = room.Interpolator(new object[] { "x" });
            interp.Dispose();
            interp.Dispose();
            Assert.IsNull(interp.Sample("x"));
            var second = room.Interpolator(new object[] { "x" });
            c.client.Shutdown();
            Assert.IsNull(second.Sample("x"), "disposed with the room");
        }
    }
}
