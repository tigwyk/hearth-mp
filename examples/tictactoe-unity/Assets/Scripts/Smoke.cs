using System;
using System.Threading.Tasks;
using Hearth;
using Hearth.Generated;
using UnityEngine;

/// <summary>
/// Headless check for IL2CPP/stripped players: <c>Player -hearthSmoke [-hearthUrl http://127.0.0.1:8787]</c>
/// plays a full typed game with two in-process clients and exits with 0 (X won)
/// or 1 (anything else).
/// </summary>
public static class Smoke
{
    [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.AfterSceneLoad)]
    static void Run()
    {
        var args = Environment.GetCommandLineArgs();
        if (Array.IndexOf(args, "-hearthSmoke") < 0)
            return;
        var i = Array.IndexOf(args, "-hearthUrl");
        var url = i >= 0 && i + 1 < args.Length ? args[i + 1] : Environment.GetEnvironmentVariable("HEARTH_URL") ?? "http://127.0.0.1:8787";
        _ = Play(url);
    }

    static async Task Play(string url)
    {
        var code = 1;
        try
        {
            var a = Client("smoke-a", url);
            var b = Client("smoke-b", url);
            await a.LoginAnonymousAsync();
            await b.LoginAnonymousAsync();
            var name = await a.CreateRoomAsync("tictactoe");
            var roomA = await a.JoinAsync<TicTacToeBoard>("tictactoe", name);
            var roomB = await b.JoinAsync<TicTacToeBoard>("tictactoe", name);
            await Until(() => roomA.State.Status?.Variant == TicTacToeStatus.Playing);
            var x = roomA.State.Turn == roomA.PlayerId ? roomA : roomB;
            var o = x == roomA ? roomB : roomA;
            foreach (var (room, cell) in new[] { (x, 0), (o, 3), (x, 1), (o, 4), (x, 2) })
            {
                room.Send(TicTacToeIntents.Place((byte)cell));
                await Until(() => room.State.Cells[cell] != 0);
            }
            await Until(() => roomB.State.Status.Variant == TicTacToeStatus.Won);
            code = roomB.State.Status.AsWon == x.PlayerId ? 0 : 1;
            Debug.Log($"hearth smoke: winner {roomB.State.Status.AsWon}, exit {code}");
        }
        catch (Exception e)
        {
            Debug.LogError("hearth smoke failed: " + e);
        }
        Application.Quit(code);
    }

    static HearthClient Client(string device, string url)
    {
        var client = new GameObject(device).AddComponent<HearthClient>();
        client.ServerUrl = url;
        client.DeviceIdOverride = device + "-" + Guid.NewGuid().ToString("N");
        return client;
    }

    static async Task Until(Func<bool> done)
    {
        var deadline = Time.realtimeSinceStartup + 20f;
        while (!done())
        {
            if (Time.realtimeSinceStartup > deadline)
                throw new TimeoutException("smoke step timed out");
            await Task.Yield();
        }
    }
}
