using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using NUnit.Framework;
using UnityEngine;

namespace Hearth.Tests
{
    /// <summary><see cref="HearthClient"/> against a <see cref="FakeTransport"/> and the real native core (JSON frames).</summary>
    public class ClientLogicTests
    {
        internal GameObject go;
        internal HearthClient client;
        internal readonly List<FakeTransport> sockets = new List<FakeTransport>();
        internal FakeTransport Socket => sockets[sockets.Count - 1];

        [SetUp]
        public void SetUp()
        {
            sockets.Clear();
            go = new GameObject("hearth-mp-test");
            client = go.AddComponent<HearthClient>();
            client.ServerUrl = "http://127.0.0.1:8787";
            client.Token = "tok";
            client.TransportFactory = () =>
            {
                var socket = new FakeTransport();
                sockets.Add(socket);
                return socket;
            };
            client.CoreFactory = hash => new NativeCore(hash, 1);
        }

        [TearDown]
        public void TearDown()
        {
            client.Shutdown();
            Object.DestroyImmediate(go);
        }

        internal HearthRoom Join(string state = "{\"cells\":[0,0,0]}")
        {
            _ = client.JoinAsync("tictactoe", "r1");
            Socket.Push(TransportEvent.Opened());
            Socket.Server("{\"op\":\"JOIN_OK\",\"player_id\":\"p1\",\"seat\":0}");
            Socket.Server("{\"op\":\"FULL_STATE\",\"seq\":1,\"state\":" + state + "}");
            client.Tick();
            Assert.IsNotNull(client.Room, "joined");
            return client.Room;
        }

        [Test]
        public void Join_Fails_With_Timeout_When_The_Server_Never_Answers()
        {
            client.JoinTimeoutMs = 0;
            var join = client.JoinAsync("tictactoe", "r1");
            Socket.Push(TransportEvent.Opened());
            client.Tick();
            Assert.IsTrue(join.IsFaulted, "a silent server fails the join");
            Assert.AreEqual("TIMEOUT", ((HearthException)join.Exception.InnerException).Code);
            Assert.IsNull(client.Room);
        }

        [Test]
        public void Join_Connects_With_Token_And_Sends_Hello()
        {
            client.SchemaHash = "00000000000000aa";
            var room = Join();
            StringAssert.StartsWith("ws://127.0.0.1:8787/rooms/tictactoe/r1?token=tok", Socket.Url);
            StringAssert.Contains("\"op\":\"HELLO\"", Socket.SentText(0));
            StringAssert.Contains("00000000000000aa", Socket.SentText(0));
            Assert.AreEqual("p1", room.PlayerId);
            Assert.AreEqual(0, room.Seat);
            Assert.AreEqual(1UL, room.StateSeq);
            Assert.AreEqual(0, (int)room.RawState["cells"][0]);
        }

        [Test]
        public void Error_Before_Join_Faults_JoinAsync()
        {
            var join = client.JoinAsync("tictactoe", "r1");
            Socket.Push(TransportEvent.Opened());
            Socket.Server("{\"op\":\"ERROR\",\"code\":\"ROOM_FULL\",\"message\":\"full\",\"ref_seq\":null}");
            client.Tick();
            Assert.AreEqual(TaskStatus.Faulted, join.Status, join.Exception?.ToString());
            Assert.AreEqual("ROOM_FULL", ((HearthException)join.Exception.InnerException).Code);
            Assert.IsNull(client.Room);
            Assert.AreEqual((ushort)1000, Socket.ClosedWith);
        }

        [Test]
        public void Send_Batches_Intents_After_Join()
        {
            var room = Join();
            Assert.AreEqual(1u, room.Send(TicTacToeIntent(0)));
            Assert.AreEqual(2u, room.Send(TicTacToeIntent(1)));
            client.Tick();
            StringAssert.Contains("\"op\":\"INTENT\"", Socket.SentText(1));
            StringAssert.Contains("\"seq\":2", Socket.SentText(1));
        }

        static Newtonsoft.Json.Linq.JToken TicTacToeIntent(int cell) =>
            Newtonsoft.Json.Linq.JToken.Parse("{\"Place\":{\"cell\":" + cell + "}}");

        [Test]
        public void Events_DispatchOnlyFromUpdate()
        {
            var room = Join();
            var mainThread = Thread.CurrentThread.ManagedThreadId;
            int? handlerThread = null;
            room.OnEvent += (name, data) => handlerThread = Thread.CurrentThread.ManagedThreadId;

            Task.Run(() => Socket.Server("{\"op\":\"EVENT\",\"name\":\"boom\",\"data\":1}")).Wait();
            Assert.IsNull(handlerThread, "nothing runs until Update");
            client.Tick();
            Assert.AreEqual(mainThread, handlerThread);
        }

        [Test]
        public void NoDispatchAfterDestroy()
        {
            var room = Join();
            var calls = 0;
            room.OnEvent += (name, data) => calls++;
            var socket = Socket;
            client.Shutdown();
            socket.Server("{\"op\":\"EVENT\",\"name\":\"late\",\"data\":1}");
            client.Tick();
            Assert.AreEqual(0, calls);
            Assert.IsTrue(socket.Disposed);
            StringAssert.Contains("\"op\":\"LEAVE\"", socket.SentText(socket.Sent.Count - 1));
            Assert.Throws<System.InvalidOperationException>(() => room.Send(1));
        }

        [Test]
        public void CoreDisposedExactlyOnce()
        {
            Join();
            client.Shutdown();
            client.Shutdown();
            Object.DestroyImmediate(go);
            client.Shutdown();
        }

        [Test]
        public void FatalCloseDoesNotReconnect()
        {
            var room = Join();
            var disconnects = new List<ushort>();
            var reconnecting = 0;
            room.OnDisconnected += (code, reason) => disconnects.Add(code);
            room.OnReconnecting += (a, ms) => reconnecting++;
            Socket.Push(TransportEvent.Closed(4001, "schema"));
            client.Tick();
            Thread.Sleep(50);
            client.Tick();
            CollectionAssert.AreEqual(new ushort[] { 4001 }, disconnects);
            Assert.AreEqual(0, reconnecting);
            Assert.AreEqual(1, sockets.Count, "never connects again");
        }

        [Test]
        public void RetryableCloseSchedulesReconnect_ThenResumes()
        {
            var room = Join();
            room.Send(1);
            client.Tick();
            var retries = new List<(int, int)>();
            uint[] unconfirmed = null;
            var reconnected = 0;
            room.OnReconnecting += (attempt, ms) => retries.Add((attempt, ms));
            room.OnIntentsUnconfirmed += seqs => unconfirmed = seqs;
            room.OnReconnected += () => reconnected++;

            Socket.Push(TransportEvent.Closed(1006, ""));
            client.Tick();
            CollectionAssert.AreEqual(new[] { (0, 500) }, retries);
            CollectionAssert.AreEqual(new uint[] { 1 }, unconfirmed);
            Assert.IsTrue(room.Reconnecting);

            Thread.Sleep(550);
            client.Tick();
            Assert.AreEqual(2, sockets.Count, "retried");
            Socket.Push(TransportEvent.Opened());
            Socket.Server("{\"op\":\"JOIN_OK\",\"player_id\":\"p1\",\"seat\":0}");
            client.Tick();
            Assert.AreEqual(1, reconnected);
            Assert.IsFalse(room.Reconnecting);
            StringAssert.Contains("\"op\":\"HELLO\"", Socket.SentText(0));
        }

        [Test]
        public void DeviceId_FallsBackToPersistedGuid()
        {
            string store = null;
            var first = Auth.DeviceId("", () => store, v => store = v);
            Assert.IsNotEmpty(first);
            Assert.AreEqual(first, store);
            Assert.AreEqual(first, Auth.DeviceId(SystemInfo.unsupportedIdentifier, () => store, v => store = v));
            Assert.AreEqual("hw-1", Auth.DeviceId("hw-1", () => store, v => store = v));
        }

        [Test]
        public void RoomUrl_Maps_Scheme_And_Escapes()
        {
            Assert.AreEqual("wss://x.dev/rooms/t/a%20b?token=a%2Bb",
                Auth.RoomUrl("https://x.dev/", "t", "a b", "a+b"));
        }
    }
}
