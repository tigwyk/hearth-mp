using System;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using UnityEngine;

/// <summary>
/// The A/B prediction checks from the Godot <c>predict_smoke.gd</c>, run by the
/// PlayMode test and by a player started with <c>-hearthSmoke -arena</c>. Client A
/// has 150 ms of one-way latency injected, B none. Needs a development or editor
/// build (for <c>DebugLatencyMs</c>) and <c>wrangler dev</c> in <c>examples/arena</c>.
/// </summary>
public static class ArenaScenario
{
    const int LatencyMs = 150;
    const int TickMs = 50;

    /// <summary>Null when every check passed, otherwise the first failure.</summary>
    public static async Task<string> Run(string url, Action<string> log)
    {
        var a = NewArena("arena-a", url);
        var b = NewArena("arena-b", url);
        string error = null;
        var ackedMax = 0u;
        var lastSeq = 0u;
        try
        {
#if UNITY_EDITOR || DEVELOPMENT_BUILD
            a.Client.DebugLatencyMs = LatencyMs;
#else
            return "needs a development build (DebugLatencyMs)";
#endif
            await a.Connect();
            b.room = a.room;
            await b.Connect();
            a.Room.OnError += (code, message, _) => error ??= $"client error {code}: {message}";
            a.Room.OnAck += (_, seqs) =>
            {
                foreach (var s in seqs)
                    ackedMax = Math.Max(ackedMax, s);
            };
            if (!await Until(() => a.Room.State.Avatars.Count >= 2 && b.Room.State.Avatars.Count >= 2, 10000))
                return "the clients never saw both avatars";

            var me = a.Room.PlayerId;
            Vector2? Predicted() => Arena.PosOf(a.Predictor.State, me);
            Vector2? Authoritative() => Arena.PosOf(a.Room.State, me);
            bool Converged(float tolerance) =>
                Predicted() is Vector2 p && Authoritative() is Vector2 q && Vector2.Distance(p, q) <= tolerance;
            void Send(int x) => lastSeq = a.Move(new Vector2Int(x, 0));

            // 1. The prediction (and the drawn avatar) moves at once; the server has not answered.
            var predictedBefore = Predicted().Value;
            var authoritativeBefore = Authoritative().Value;
            var drawnBefore = a.DrawnSelf;
            Send(1);
            if (Predicted().Value.x <= predictedBefore.x + 0.1f)
                return $"prediction did not move on the first frame ({predictedBefore.x:F3} -> {Predicted().Value.x:F3})";
            if (Authoritative().Value.x > authoritativeBefore.x + 0.001f)
                return "the authoritative state moved before the round trip";
            await Task.Yield();
            await Task.Yield();
            if (a.DrawnSelf.x <= drawnBefore.x + 0.005f)
                return $"the drawn avatar did not move on the first frame ({drawnBefore.x:F3} -> {a.DrawnSelf.x:F3})";
            log("ok - prediction and drawn avatar moved on the first frame");

            // 2. Stop: prediction matches the server within 0.01 within 300 ms of the last ACK.
            for (var i = 0; i < 9; i++)
            {
                await Wait(TickMs);
                Send(1);
            }
            Send(0);
            if (!await Until(() => ackedMax >= lastSeq, 3000))
                return $"the last input was never acknowledged ({ackedMax} of {lastSeq})";
            if (!await Until(() => Converged(0.01f), 300))
                return "prediction did not match the authoritative position within 300 ms of the last ACK";
            log("ok - converged after stop");

            // 3. B blocks A: the prediction overlaps B, then the correction lands within 200 ms of the ACK.
            var overlapped = false;
            var maxDivergence = 0f;
            for (var i = 0; i < 30; i++)
            {
                Send(1);
                await Wait(TickMs);
                if (Predicted() is Vector2 p && Authoritative() is Vector2 q)
                    maxDivergence = Math.Max(maxDivergence, Vector2.Distance(p, q));
                if (Predicted() is Vector2 p2 && Arena.PosOf(a.Room.State, b.Room.PlayerId) is Vector2 other && Vector2.Distance(p2, other) < 1f)
                    overlapped = true;
            }
            Send(0);
            if (!await Until(() => ackedMax >= lastSeq, 3000))
                return "the last blocking input was never acknowledged";
            if (!await Until(() => Converged(0.05f), 200))
                return "prediction was not within 0.05 of the authoritative position within 200 ms of the last ACK";
            if (!overlapped)
                return "A never overlapped B, so the contact correction was not exercised";
            if (maxDivergence <= 0.05f)
                return "A's prediction never diverged from the authoritative state";
            log($"ok - blocked path: diverged {maxDivergence:F2} units, then corrected");

            // 4. A raw [5,0] direction moves the normal one-tick distance (server-side clamp).
#if UNITY_EDITOR || DEVELOPMENT_BUILD
            a.Client.DebugLatencyMs = 0;
#endif
            await Wait(500);
            for (var i = 0; i < 25; i++)
            {
                Send(-1);
                await Wait(TickMs);
            }
            Send(0);
            if (!await Until(() => ackedMax >= lastSeq, 3000))
                return "the last separating input was never acknowledged";
            if (!await Stable(() => Authoritative().Value, 2000))
                return "A's authoritative position never settled before the raw move";
            var before = Authoritative().Value;
            a.Room.Send(JToken.Parse("{\"Move\":{\"dir\":[5,0]}}"));
            Vector2? after = null;
            await Until(() => Math.Abs(Authoritative().Value.x - before.x) > 0.02f && (after = Authoritative()) != null, 3000);
            if (after == null)
                return "the server never moved A for the raw [5,0] intent";
            var delta = after.Value.x - before.x;
            if (delta < 0.1f || delta > 0.35f)
                return $"the server moved {delta:F2} units for a clamped [5,0] direction, expected about 0.2";
            log($"ok - raw [5,0] moved A {delta:F2} units");
            return error;
        }
        catch (Exception e)
        {
            return e.ToString();
        }
        finally
        {
            UnityEngine.Object.Destroy(a.gameObject);
            UnityEngine.Object.Destroy(b.gameObject);
        }
    }

    static Arena NewArena(string name, string url)
    {
        var go = new GameObject(name);
        go.SetActive(false);
        var client = go.AddComponent<Hearth.HearthClient>();
        client.ServerUrl = url;
        client.DeviceIdOverride = name + "-" + Guid.NewGuid().ToString("N");
        var arena = go.AddComponent<Arena>();
        arena.connectOnStart = false;
        arena.readKeyboard = false;
        go.SetActive(true);
        return arena;
    }

    static async Task Wait(int ms)
    {
        var deadline = Time.realtimeSinceStartup + ms / 1000f;
        while (Time.realtimeSinceStartup < deadline)
            await Task.Yield();
    }

    static async Task<bool> Until(Func<bool> done, int timeoutMs)
    {
        var deadline = Time.realtimeSinceStartup + timeoutMs / 1000f;
        while (Time.realtimeSinceStartup < deadline)
        {
            if (done())
                return true;
            await Task.Yield();
        }
        return done();
    }

    /// <summary>Whether the value stays put for ~100 ms.</summary>
    static async Task<bool> Stable(Func<Vector2> value, int timeoutMs)
    {
        var deadline = Time.realtimeSinceStartup + timeoutMs / 1000f;
        var last = value();
        var since = Time.realtimeSinceStartup;
        while (Time.realtimeSinceStartup < deadline)
        {
            await Task.Yield();
            var now = value();
            if (Vector2.Distance(now, last) < 0.001f)
            {
                if (Time.realtimeSinceStartup - since >= 0.1f)
                    return true;
            }
            else
            {
                since = Time.realtimeSinceStartup;
            }
            last = now;
        }
        return false;
    }
}
