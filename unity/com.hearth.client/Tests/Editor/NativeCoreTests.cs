using System.IO;
using System.Text;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Hearth.Tests
{
    /// <summary>The native plugin through <see cref="NativeCore"/>, with hand-written JSON frames.</summary>
    public class NativeCoreTests
    {
        const string Hash = "0123456789abcdef";

        static byte[] Text(string s) => Encoding.UTF8.GetBytes(s);

        [Test]
        public void Hello_Then_Handshake_RoundTrip()
        {
            using var core = new NativeCore(Hash, 1);
            var hello = JObject.Parse(Encoding.UTF8.GetString(core.Hello("{\"name\":\"ana\"}", out var text)));
            Assert.IsTrue(text);
            Assert.AreEqual("HELLO", (string)hello["op"]);
            Assert.AreEqual(Hash, (string)hello["schema_hash"]);
            Assert.AreEqual("ana", (string)hello["opts"]["name"]);

            var events = core.Handle(Text("{\"op\":\"JOIN_OK\",\"player_id\":\"p1\",\"seat\":1}"), true);
            Assert.AreEqual("joined", (string)events[0]["type"]);
            Assert.AreEqual("p1", (string)events[0]["player_id"]);
            events = core.Handle(Text("{\"op\":\"FULL_STATE\",\"seq\":3,\"state\":{\"cells\":[0,1]}}"), true);
            Assert.AreEqual("state_reset", (string)events[0]["type"]);
            Assert.IsTrue(JToken.DeepEquals(JToken.Parse("{\"cells\":[0,1]}"), core.State()));
            Assert.AreEqual(3UL, core.Seq);
        }

        [Test]
        public void Queue_And_Poll_Batches()
        {
            using var core = new NativeCore(Hash, 1);
            Assert.AreEqual(1u, core.QueueIntent(JToken.Parse("{\"Place\":{\"cell\":0}}")));
            Assert.AreEqual(2u, core.QueueIntent("Rematch"));
            Assert.AreEqual(3u, core.QueueIntent(7));
            var frame = JObject.Parse(Encoding.UTF8.GetString(core.PollSend(0, out var text)));
            Assert.IsTrue(text);
            Assert.AreEqual("INTENT", (string)frame["op"]);
            Assert.AreEqual(3, ((JArray)frame["intents"]).Count);
            Assert.IsNull(core.PollSend(0, out _));
        }

        [Test]
        public void Garbage_Frame_Is_Error_Event()
        {
            using var core = new NativeCore(Hash, 0);
            var events = core.Handle(new byte[] { 0xFF, 0x00 }, false);
            Assert.AreEqual("error", (string)events[0]["type"]);
            Assert.AreEqual("BAD_FRAME", (string)events[0]["code"]);
        }

        [Test]
        public void Null_Handle_Returns_Zero_With_Plugin_Message()
        {
            using var none = new HearthCoreHandle();
            Assert.AreEqual(0u, NativeCore.Native.hearth_client_queue_intent(none, Json.Utf8Z("1")));
            StringAssert.Contains("null", NativeCore.Native.LastError());
            Assert.IsNull(NativeCore.Native.Take(NativeCore.Native.hearth_client_state(none)));
        }

        [Test]
        public void ReconnectDecide_MatchesFixture()
        {
            var path = Path.Combine(Path.GetFullPath("Packages/com.hearth.client"),
                "../../crates/hearth-client/tests/fixtures/reconnect_cases.json");
            var cases = JArray.Parse(File.ReadAllText(path));
            Assert.IsNotEmpty(cases);
            foreach (var c in cases)
            {
                var actual = HearthNative.ReconnectDecide((ushort)c["code"], (ulong)c["first_drop_ms"], (uint)c["attempt"], (ulong)c["now_ms"]);
                var expect = c["expect"].Type == JTokenType.String ? -1 : (long)c["expect"];
                Assert.AreEqual(expect, actual, (string)c["name"]);
            }
        }

        [Test]
        public void Dispose_Twice_IsSafe()
        {
            var core = new NativeCore(Hash, 0);
            core.Dispose();
            core.Dispose();
        }
    }
}
