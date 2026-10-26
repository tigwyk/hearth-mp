using System;
using System.Linq;
using Newtonsoft.Json.Linq;

namespace Hearth
{
    /// <summary>
    /// Predicts your own player with the game's shared Rust logic, compiled into
    /// the native plugin (<c>hearth_mp_unity_core::plugin! { logic: { "arena" => ... } }</c>).
    /// <see cref="State"/> is a typed mirror of the predicted state, with the same
    /// <c>OnChange</c> events as <c>room.State</c>.
    /// </summary>
    /// <remarks>
    /// Each <see cref="Input"/> is sent to the room and applied locally at once.
    /// On every patch or <c>ACK</c> the prediction is rebuilt from the
    /// authoritative state and the unacknowledged inputs are replayed. If the
    /// logic fails (its state does not match the room's, or it panics) the
    /// predictor disables itself, raises <see cref="OnError"/> once, and
    /// <see cref="State"/> follows the authoritative state from then on.
    /// </remarks>
    public sealed class HearthPredictor<TState> : IDisposable where TState : HearthNode, new()
    {
        readonly HearthRoom room;
        IPredictorBackend backend;

        /// <summary>The predicted state (the authoritative state once poisoned).</summary>
        public TState State { get; } = new TState();

        /// <summary>True once the logic failed; the predictor then only mirrors the room.</summary>
        public bool Poisoned { get; private set; }

        /// <summary>Raised once, when the predictor is poisoned.</summary>
        public event Action<string> OnError;

        /// <param name="room">A joined room.</param>
        /// <param name="logicName">The name the plugin registered the logic under.</param>
        /// <param name="tickHz">The room's tick rate.</param>
        /// <exception cref="HearthException"><c>PREDICTOR_UNAVAILABLE</c> when the native plugin has no such logic.</exception>
        public HearthPredictor(HearthRoom room, string logicName, int tickHz)
        {
            this.room = room ?? throw new ArgumentNullException(nameof(room));
            if (string.IsNullOrEmpty(room.PlayerId))
                throw new InvalidOperationException("join the room before creating a predictor");
            try
            {
                backend = PredictorBackend.Create(logicName, room.PlayerId, tickHz);
            }
            catch (Exception e) when (e is EntryPointNotFoundException || e is DllNotFoundException)
            {
                throw Unavailable(logicName, "the native plugin has no prediction exports (" + e.Message + ")");
            }
            if (backend == null)
                throw Unavailable(logicName, PredictorBackend.LastError());

            room.OnStateReset += OnReset;
            room.OnPatched += OnPatched;
            room.OnAck += OnAck;
            room.OnIntentsUnconfirmed += OnUnconfirmed;
            room.OnDisconnected += OnGaveUp;
            room.Closed += Dispose;
            Reconcile(Array.Empty<uint>());
        }

        static HearthException Unavailable(string logic, string why) => new HearthException(
            "PREDICTOR_UNAVAILABLE",
            $"{why}; rebuild the native plugin with plugin! {{ logic: {{ \"{logic}\" => ... }} }}");

        /// <summary>
        /// Send <paramref name="intent"/> and predict it at once. Returns the seq
        /// <see cref="HearthRoom.Send"/> returned, which later ACKs refer to.
        /// </summary>
        /// <param name="logicInput">The logic's input, when it differs from the room
        /// intent (for example the intent <c>{"Move": {...}}</c> wraps the logic
        /// input <c>{...}</c>). Defaults to <paramref name="intent"/>.</param>
        public uint Input(JToken intent, JToken logicInput = null)
        {
            var seq = room.Send(intent);
            if (backend != null && !Poisoned)
            {
                if (backend.Input(seq, Json.Compact(logicInput ?? intent)))
                    Reload();
                else
                    Poison();
            }
            return seq;
        }

        void OnReset() => Reconcile(Array.Empty<uint>());
        void OnPatched(JArray ops) => Reconcile(Array.Empty<uint>());
        void OnAck(ulong stateSeq, uint[] seqs) => Reconcile(seqs);

        void OnUnconfirmed(uint[] seqs)
        {
            if (backend != null && !Poisoned)
                backend.OnUnconfirmed(new JArray(seqs.Cast<object>().ToArray()).ToString(Newtonsoft.Json.Formatting.None));
        }

        void OnGaveUp(ushort code, string reason) => Dispose();

        void Reconcile(uint[] acked)
        {
            if (backend == null)
                return;
            if (Poisoned)
            {
                State.Load(room.RawState);
                return;
            }
            var ackedJson = new JArray(acked.Cast<object>().ToArray()).ToString(Newtonsoft.Json.Formatting.None);
            if (backend.OnAuthoritative(Json.Compact(room.RawState), ackedJson))
                Reload();
            else
                Poison();
        }

        void Reload()
        {
            var state = backend.State();
            if (string.IsNullOrEmpty(state))
                Poison();
            else
                State.Load(JToken.Parse(state));
        }

        void Poison()
        {
            if (Poisoned)
                return;
            Poisoned = true;
            var message = PredictorBackend.LastError();
            State.Load(room.RawState);
            OnError?.Invoke(message);
        }

        /// <summary>Unsubscribe and free the native predictor. Idempotent; nothing fires afterwards.</summary>
        public void Dispose()
        {
            if (backend == null)
                return;
            room.OnStateReset -= OnReset;
            room.OnPatched -= OnPatched;
            room.OnAck -= OnAck;
            room.OnIntentsUnconfirmed -= OnUnconfirmed;
            room.OnDisconnected -= OnGaveUp;
            room.Closed -= Dispose;
            backend.Dispose();
            backend = null;
        }
    }
}
