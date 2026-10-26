using System;
using UnityEngine;

/// <summary>
/// Headless check for stripped/IL2CPP development players:
/// <c>Player -hearthSmoke -arena [-hearthUrl http://127.0.0.1:8787]</c> runs
/// <see cref="ArenaScenario"/> and exits 0 when every check passed, 1 otherwise.
/// </summary>
public static class ArenaSmoke
{
    [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.AfterSceneLoad)]
    static async void Run()
    {
        var args = Environment.GetCommandLineArgs();
        if (Array.IndexOf(args, "-hearthSmoke") < 0 || Array.IndexOf(args, "-arena") < 0)
            return;
        foreach (var arena in UnityEngine.Object.FindObjectsByType<Arena>(FindObjectsSortMode.None))
            arena.connectOnStart = false;
        var i = Array.IndexOf(args, "-hearthUrl");
        var url = i >= 0 && i + 1 < args.Length ? args[i + 1] : Environment.GetEnvironmentVariable("HEARTH_URL") ?? "http://127.0.0.1:8787";
        var failure = await ArenaScenario.Run(url, Debug.Log);
        Debug.Log(failure == null ? "arena smoke: all checks passed" : "arena smoke FAILED: " + failure);
        Application.Quit(failure == null ? 0 : 1);
    }
}
