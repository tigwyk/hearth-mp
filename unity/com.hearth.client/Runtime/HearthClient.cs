using System;
using System.Diagnostics;
using System.Linq;
using System.Reflection;
using System.Threading.Tasks;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;
using UnityEngine;

namespace Hearth
{
    /// <summary>
    /// The Hearth client: log in, create or join a room, and drive its socket.
    /// Everything (state events, join completion) happens on the main thread
    /// in <c>Update</c>.
    /// </summary>
    /// <example><code>
    /// await client.LoginAnonymousAsync();
    /// var name = await client.CreateRoomAsync("tictactoe");
    /// var room = await client.JoinAsync&lt;TicTacToeBoard&gt;("tictactoe", name);
    /// room.State.OnStatusChange += status =&gt; ...;
    /// </code></example>
    public class HearthClient : MonoBehaviour
    {
        /// <summary>Gateway base URL, for example <c>http://127.0.0.1:8787</c>.</summary>
        [SerializeField] string serverUrl = "http://127.0.0.1:8787";

        /// <summary>The room state's schema hash, for the untyped <see cref="JoinAsync(string, string, JObject)"/>.</summary>
        [SerializeField] string schemaHash = "";

        /// <summary>Set by the editor's local-server toggle; used instead of the server URL when non-null.</summary>
        public static string UrlOverride;

        public string ServerUrl
        {
            get => UrlOverride ?? serverUrl;
            set => serverUrl = value;
        }

        public string SchemaHash
        {
            get => schemaHash;
            set => schemaHash = value;
        }

        /// <summary>Test seam: the device id for anonymous login (two clients on one machine).</summary>
        public string DeviceIdOverride { get; set; }

        /// <summary>The gateway token from the last login; null before.</summary>
        public string Token { get; internal set; }

        /// <summary>The player id from the last login; null before.</summary>
        public string PlayerId { get; private set; }

        /// <summary>The current room, or null.</summary>
        public HearthRoom Room { get; private set; }

        /// <summary>True once the socket has joined and not given up.</summary>
        public bool Connected => transport != null && joined;

        internal Func<IHearthTransport> TransportFactory = DefaultTransport;
        internal Func<string, IHearthCore> CoreFactory = DefaultCore;

        IHearthCore core;
        IHearthTransport transport;
        string roomUrl;
        string optsJson;
        bool helloSent;
        bool joined;
        bool disposed;
        readonly Reconnect reconnect = new Reconnect();
        Stopwatch clock;

        /// <summary>Set while <see cref="JoinAsync(string, string, JObject)"/> waits for <c>JOIN_OK</c> + <c>FULL_STATE</c>.</summary>
        TaskCompletionSource<HearthRoom> pendingJoin;
        HearthRoom pendingRoom;

        internal long NowMs => (clock ??= Stopwatch.StartNew()).ElapsedMilliseconds;

        internal ulong Seq => core != null && !disposed ? core.Seq : 0;

        static IHearthTransport DefaultTransport()
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            return new WebGLTransport();
#else
            return new ClientWebSocketTransport();
#endif
        }

        static IHearthCore DefaultCore(string hash)
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            return new WebGLCore(hash);
#else
            return new NativeCore(hash);
#endif
        }

        void Awake() => clock ??= Stopwatch.StartNew();

        /// <summary>
        /// <c>POST /auth/anonymous</c>. The device id is <see cref="SystemInfo.deviceUniqueIdentifier"/>,
        /// or a GUID kept in <c>PlayerPrefs["hearth_device_id"]</c> where the platform has none.
        /// </summary>
        public async Task LoginAnonymousAsync()
        {
            Token = null;
            PlayerId = null;
            var deviceId = string.IsNullOrEmpty(DeviceIdOverride) ? Auth.DeviceId() : DeviceIdOverride;
            var body = new JObject { ["device_id"] = deviceId }.ToString(Formatting.None);
            var reply = await Auth.PostAsync(ServerUrl.TrimEnd('/') + "/auth/anonymous", body, null);
            var token = (string)reply["token"];
            if (string.IsNullOrEmpty(token))
                throw new HearthException("AUTH_FAILED", "POST /auth/anonymous did not return a token");
            Token = token;
            PlayerId = (string)reply["player_id"];
        }

        /// <summary><c>POST /rooms/{roomType}</c>: mint a room name.</summary>
        public async Task<string> CreateRoomAsync(string roomType)
        {
            RequireLogin();
            var reply = await Auth.PostAsync($"{ServerUrl.TrimEnd('/')}/rooms/{Uri.EscapeDataString(roomType)}", "", Token);
            var room = (string)reply["room"];
            if (string.IsNullOrEmpty(room))
                throw new HearthException("BAD_RESPONSE", $"POST /rooms/{roomType} did not return a room");
            return room;
        }

        /// <summary>
        /// Join a room with the untyped API (<see cref="HearthRoom.RawState"/>).
        /// Completes on <c>JOIN_OK</c> + <c>FULL_STATE</c>; faults with
        /// <see cref="HearthException"/> on an <c>ERROR</c> or a close before that.
        /// </summary>
        public Task<HearthRoom> JoinAsync(string roomType, string room, JObject opts = null) =>
            JoinCore(roomType, room, opts, schemaHash, new HearthRoom());

        /// <summary>
        /// Join with a generated state type: <c>room.State</c> is a typed mirror
        /// with <c>OnChange</c> events. The schema hash comes from <c>[HearthSchema]</c>
        /// on <typeparamref name="TState"/>.
        /// </summary>
        public async Task<HearthRoom<TState>> JoinAsync<TState>(string roomType, string room, JObject opts = null)
            where TState : HearthNode, new()
        {
            var attribute = typeof(TState).GetCustomAttribute<HearthSchemaAttribute>();
            if (attribute == null)
                throw new InvalidOperationException("TState has no [HearthSchema]; run hearth codegen --unity");
            return (HearthRoom<TState>)await JoinCore(roomType, room, opts, attribute.Hash, new HearthRoom<TState>());
        }

        Task<HearthRoom> JoinCore(string roomType, string room, JObject opts, string hash, HearthRoom target)
        {
            RequireLogin();
            if (disposed)
                throw new ObjectDisposedException(nameof(HearthClient));
#if UNITY_WEBGL && !UNITY_EDITOR
            return JoinWhenReady();
            async Task<HearthRoom> JoinWhenReady()
            {
                await WebGLCore.WhenReady();
                return await StartJoin(roomType, room, opts, hash, target);
            }
#else
            return StartJoin(roomType, room, opts, hash, target);
#endif
        }

        Task<HearthRoom> StartJoin(string roomType, string room, JObject opts, string hash, HearthRoom target)
        {
            CloseRoom(sendLeave: true);
            core = CoreFactory(hash);
            roomUrl = Auth.RoomUrl(ServerUrl, roomType, room, Token);
            optsJson = opts == null ? "{}" : opts.ToString(Formatting.None);
            target.Client = this;
            target.RoomType = roomType;
            target.Name = room;
            pendingRoom = target;
            pendingJoin = new TaskCompletionSource<HearthRoom>(TaskCreationOptions.RunContinuationsAsynchronously);
            reconnect.Reset();
            OpenSocket();
            return pendingJoin.Task;
        }

        /// <summary>Send <c>LEAVE</c> and close the socket. <c>OnDisconnected</c> does not fire for it.</summary>
        public void Leave() => CloseRoom(sendLeave: true);

        internal uint QueueIntent(JToken intent)
        {
            if (disposed || core == null)
                throw new InvalidOperationException("the room is closed");
            return core.QueueIntent(intent);
        }

        void RequireLogin()
        {
            if (string.IsNullOrEmpty(Token))
                throw new InvalidOperationException("call LoginAnonymousAsync() first");
        }

        void Update() => Tick();

        /// <summary>One frame: drain the transport, dispatch, send, run the retry timer.</summary>
        internal void Tick()
        {
            if (disposed || core == null)
                return;
            var now = NowMs;
            if (transport == null)
            {
                if (!reconnect.TakeDue(now))
                    return;
                OpenSocket();
            }
            while (transport != null && transport.TryReceive(out var ev))
            {
                switch (ev.Kind)
                {
                    case TransportEventKind.Opened:
                        var hello = core.Hello(optsJson, out var helloText);
                        transport.Send(hello, helloText);
                        helloSent = true;
                        break;
                    case TransportEventKind.Message:
                        Dispatch(core.Handle(ev.Data, ev.Text));
                        break;
                    case TransportEventKind.Closed:
                        transport.Dispose();
                        transport = null;
                        HandleDrop(ev.Code, ev.Reason, now);
                        break;
                }
                // A handler may have destroyed the client or left the room.
                if (disposed || core == null)
                    return;
            }
            if (transport == null || !helloSent)
                return;
            byte[] frame;
            while ((frame = core.PollSend((ulong)now, out var text)) != null)
                transport.Send(frame, text);
        }

        void OpenSocket()
        {
            helloSent = false;
            transport = TransportFactory();
            transport.Connect(roomUrl);
        }

        void HandleDrop(ushort code, string reason, long now)
        {
            joined = false;
            if (pendingJoin != null)
            {
                // Never joined: nothing to resume.
                Fail(new HearthException("DISCONNECTED", $"socket closed before the join ({code} {reason})".TrimEnd()));
                return;
            }
            var room = Room;
            if (room == null)
                return;
            Dispatch(core.OnDisconnected());
            var retryIn = reconnect.OnDrop(code, now);
            if (retryIn < 0)
            {
                room.Reconnecting = false;
                room.RaiseDisconnected(code, reason);
                return;
            }
            room.Reconnecting = true;
            room.RaiseReconnecting((int)reconnect.Attempt - 1, (int)retryIn);
        }

        void Dispatch(JArray events)
        {
            foreach (var ev in events)
            {
                if (disposed || core == null)
                    return;
                var room = Room ?? pendingRoom;
                switch ((string)ev["type"])
                {
                    case "joined":
                        joined = true;
                        var resuming = reconnect.Resuming;
                        reconnect.Reset();
                        room.PlayerId = (string)ev["player_id"];
                        room.Seat = (int)ev["seat"];
                        room.Reconnecting = false;
                        if (resuming && room == Room)
                            room.RaiseReconnected();
                        break;
                    case "state_reset":
                        room.ApplyReset(core.State());
                        if (pendingJoin != null && joined)
                        {
                            Room = pendingRoom;
                            pendingRoom = null;
                            var join = pendingJoin;
                            pendingJoin = null;
                            join.TrySetResult(Room);
                        }
                        break;
                    case "patched":
                        // shortcut: refetches the whole state per patch; apply ops in C# if states get large.
                        room.ApplyPatch(core.State(), (JArray)ev["ops"]);
                        break;
                    case "acked":
                        room.RaiseAck((ulong)ev["state_seq"], ev["intent_seqs"].Select(s => (uint)s).ToArray());
                        break;
                    case "event":
                        room.RaiseEvent((string)ev["name"], ev["data"]);
                        break;
                    case "error":
                        var code = (string)ev["code"];
                        var message = (string)ev["message"];
                        if (pendingJoin != null)
                            Fail(new HearthException(code, message));
                        else
                            room.RaiseError(code, message, ev["ref_seq"]?.Type == JTokenType.Integer ? (uint?)(uint)ev["ref_seq"] : null);
                        break;
                    case "desync":
                        room.RaiseError("DESYNC", "patch failed to apply; waiting for FULL_STATE", null);
                        break;
                    case "intents_unconfirmed":
                        room.RaiseIntentsUnconfirmed(ev["seqs"].Select(s => (uint)s).ToArray());
                        break;
                }
            }
        }

        /// <summary>Fault the pending join and drop the socket and core.</summary>
        void Fail(Exception error)
        {
            var join = pendingJoin;
            pendingJoin = null;
            pendingRoom = null;
            CloseRoom(sendLeave: false);
            join?.TrySetException(error);
        }

#if UNITY_EDITOR || DEVELOPMENT_BUILD
        /// <summary>Drop the socket locally and handle it like an abnormal (1006) close.</summary>
        internal void DebugDrop()
        {
            if (transport == null)
                return;
            transport.Dispose();
            transport = null;
            HandleDrop(1006, "debug drop", NowMs);
        }
#endif

        /// <summary>Close the socket (with <c>LEAVE</c> when joined) and free the core.</summary>
        void CloseRoom(bool sendLeave)
        {
            if (transport != null)
            {
                if (sendLeave && joined && core != null)
                {
                    var leave = core.Leave(out var text);
                    transport.Send(leave, text);
                }
                transport.Close(1000);
                transport.Dispose();
                transport = null;
            }
            core?.Dispose();
            core = null;
            joined = false;
            reconnect.Reset();
            if (Room != null)
            {
                Room.Client = null;
                Room = null;
            }
            pendingJoin?.TrySetCanceled();
            pendingJoin = null;
            pendingRoom = null;
        }

        void OnDestroy() => Shutdown();

        /// <summary>Teardown: <c>LEAVE</c>, close, free the core; nothing fires afterwards. Idempotent.</summary>
        internal void Shutdown()
        {
            if (disposed)
                return;
            CloseRoom(sendLeave: true);
            disposed = true;
        }
    }
}
