using System.Collections.Generic;
using System.Threading;
using Hearth.Tests.Generated;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Hearth.Tests
{
    /// <summary>
    /// <see cref="HearthPredictor{TState}"/> against the stock plugin's built-in
    /// <c>hearth.echo1d</c> logic, over a scripted room.
    /// </summary>
    public class PredictorTests
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

        HearthPredictor<Echo1dState> Predictor(string state = "{\"x\":0}")
        {
            var room = c.Join(state);
            return new HearthPredictor<Echo1dState>(room, "hearth.echo1d", 20);
        }

        void Server(string json)
        {
            c.Socket.Server(json);
            c.client.Tick();
        }

        void Ack(ulong stateSeq, params uint[] seqs) =>
            Server("{\"op\":\"ACK\",\"state_seq\":" + stateSeq + ",\"intent_seqs\":[" + string.Join(",", seqs) + "]}");

        void SetX(ulong seq, int x) =>
            Server("{\"op\":\"PATCH\",\"seq\":" + seq + ",\"ops\":[{\"op\":\"set\",\"path\":[\"x\"],\"value\":" + x + "}]}");

        [Test]
        public void Input_PredictsImmediately()
        {
            var p = Predictor();
            var changes = new List<long>();
            p.State.OnXChange += changes.Add;
            Assert.AreEqual(1u, p.Input(1));
            Assert.AreEqual(1, p.State.X, "before any server frame");
            CollectionAssert.AreEqual(new long[] { 1 }, changes);
        }

        [Test]
        public void Ack_DropsAndReplays()
        {
            var p = Predictor();
            p.Input(1);
            p.Input(1);
            p.Input(1);
            Assert.AreEqual(3, p.State.X);
            // The server applied 1 and 2 and also moved x by its own rules.
            SetX(2, 10);
            Ack(2, 1, 2);
            Assert.AreEqual(11, p.State.X, "authoritative + the one unacked input");
        }

        [Test]
        public void SeqMatchesSendReturn_WhileDisconnected()
        {
            var p = Predictor();
            c.Socket.Push(TransportEvent.Closed(1006, ""));
            c.client.Tick();
            Assert.AreEqual(1u, p.Input(1));
            Assert.AreEqual(2u, p.Input(1));

            Thread.Sleep(550);
            c.client.Tick();
            c.Socket.Push(TransportEvent.Opened());
            c.Socket.Server("{\"op\":\"JOIN_OK\",\"player_id\":\"p1\",\"seat\":0}");
            c.Socket.Server("{\"op\":\"FULL_STATE\",\"seq\":1,\"state\":{\"x\":0}}");
            c.client.Tick();
            Assert.AreEqual(2, p.State.X, "both inputs replayed over the fresh state");
            SetX(2, 1);
            Ack(2, 1);
            Assert.AreEqual(2, p.State.X, "authoritative (1) + input 2");
        }

        [Test]
        public void UnknownLogic_ThrowsPredictorUnavailable_ListsNames()
        {
            var room = c.Join("{\"x\":0}");
            var e = Assert.Throws<HearthException>(() => new HearthPredictor<Echo1dState>(room, "arnea", 20));
            Assert.AreEqual("PREDICTOR_UNAVAILABLE", e.Code);
            StringAssert.Contains("unknown logic \"arnea\"", e.Message);
            StringAssert.Contains("hearth.echo1d", e.Message);
            StringAssert.Contains("plugin! { logic: { \"arnea\" => ... } }", e.Message);
        }

        [Test]
        public void BadStateShape_PoisonsOnce_FallsBackToAuthoritative()
        {
            var room = c.Join("{\"y\":1}");
            var errors = new List<string>();
            HearthPredictor<Echo1dState> p = null;
            Assert.DoesNotThrow(() => p = new HearthPredictor<Echo1dState>(room, "hearth.echo1d", 20));
            p.OnError += errors.Add;
            Assert.IsTrue(p.Poisoned, "the first reconcile already failed");
            for (var i = 0; i < 10; i++)
            {
                p.Input(1);
                Server("{\"op\":\"PATCH\",\"seq\":" + (i + 2) + ",\"ops\":[{\"op\":\"set\",\"path\":[\"y\"],\"value\":" + i + "}]}");
            }
            Assert.AreEqual(0, errors.Count, "reported at construction, never again");

            var room2Errors = new List<string>();
            c.TearDown();
            c.SetUp();
            var room2 = c.Join("{\"x\":0}");
            var p2 = new HearthPredictor<Echo1dState>(room2, "hearth.echo1d", 20);
            p2.OnError += room2Errors.Add;
            for (var i = 0; i < 10; i++)
                Server("{\"op\":\"PATCH\",\"seq\":" + (i + 2) + ",\"ops\":[{\"op\":\"set\",\"path\":[],\"value\":{\"y\":" + i + "}}]}");
            Assert.AreEqual(1, room2Errors.Count, "exactly once across 10 updates");
            StringAssert.Contains("predictor disabled", room2Errors[0]);
            Assert.IsTrue(p2.Poisoned);
            Assert.IsTrue(JToken.DeepEquals(room2.RawState, JToken.Parse("{\"y\":9}")));
            Assert.AreEqual(0, p2.State.X, "State mirrors RawState (no x)");
        }

        [Test]
        public void DisposeAfterRoomDestroyed_IsSafe_NoEvents()
        {
            var p = Predictor();
            var changes = 0;
            p.State.OnXChange += _ => changes++;
            var socket = c.Socket;
            c.client.Shutdown();
            socket.Server("{\"op\":\"PATCH\",\"seq\":2,\"ops\":[{\"op\":\"set\",\"path\":[\"x\"],\"value\":5}]}");
            c.client.Tick();
            p.Dispose();
            Assert.AreEqual(0, changes);
        }

        [Test]
        public void Dispose_Twice_Safe()
        {
            var p = Predictor();
            p.Dispose();
            p.Dispose();
            Assert.AreEqual(1u, p.Input(1), "Input still sends after Dispose");
        }
    }
}
