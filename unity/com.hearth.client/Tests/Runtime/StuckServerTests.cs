using System;
using System.Collections;
using System.Collections.Generic;
using System.Net;
using System.Net.Sockets;
using System.Threading.Tasks;
using NUnit.Framework;
using UnityEngine;
using UnityEngine.TestTools;

namespace Hearth.Tests
{
    /// <summary>
    /// A server that accepts connections but never answers must not hang the
    /// client: login fails with <c>HTTP_FAILED</c> and a join with <c>TIMEOUT</c>.
    /// The test's own <see cref="TcpListener"/> is the stuck server.
    /// </summary>
    public class StuckServerTests
    {
        TcpListener listener;
        readonly List<TcpClient> held = new List<TcpClient>();
        HearthClient client;
        int savedHttpTimeout;

        [SetUp]
        public void SetUp()
        {
            listener = new TcpListener(IPAddress.Loopback, 0);
            listener.Start();
            AcceptForever();
            savedHttpTimeout = Auth.HttpTimeoutSeconds;
            client = new GameObject("stuck-server-test").AddComponent<HearthClient>();
            client.ServerUrl = "http://127.0.0.1:" + ((IPEndPoint)listener.LocalEndpoint).Port;
            client.DeviceIdOverride = "stuck-server-device";
        }

        [TearDown]
        public void TearDown()
        {
            Auth.HttpTimeoutSeconds = savedHttpTimeout;
            UnityEngine.Object.Destroy(client.gameObject);
            listener.Stop();
            lock (held)
                foreach (var socket in held)
                    socket.Dispose();
        }

        async void AcceptForever()
        {
            try
            {
                while (true)
                {
                    var socket = await listener.AcceptTcpClientAsync();
                    lock (held)
                        held.Add(socket); // kept open, never answered
                }
            }
            catch (Exception) { } // the listener stopped
        }

        [UnityTest, Timeout(30000)]
        public IEnumerator Login_Fails_When_The_Server_Never_Answers()
        {
            Auth.HttpTimeoutSeconds = 1;
            var login = client.LoginAnonymousAsync();
            yield return WaitFor(login, 10f);
            Assert.IsTrue(login.IsFaulted, "a silent server fails the login");
            Assert.AreEqual("HTTP_FAILED", ((HearthException)login.Exception.InnerException).Code);
        }

        [UnityTest, Timeout(30000)]
        public IEnumerator Join_Fails_When_The_Server_Never_Answers()
        {
            client.Token = "tok";
            client.JoinTimeoutMs = 1000;
            var join = client.JoinAsync("tictactoe", "r1");
            yield return WaitFor(join, 10f);
            Assert.IsTrue(join.IsFaulted, "a silent server fails the join");
            Assert.AreEqual("TIMEOUT", ((HearthException)join.Exception.InnerException).Code);
            lock (held)
                Assert.IsNotEmpty(held, "the client connected to the stuck server");
        }

        static IEnumerator WaitFor(Task task, float seconds)
        {
            var deadline = Time.realtimeSinceStartup + seconds;
            while (!task.IsCompleted && Time.realtimeSinceStartup < deadline)
                yield return null;
            Assert.IsTrue(task.IsCompleted, $"still waiting after {seconds} s");
        }
    }
}
