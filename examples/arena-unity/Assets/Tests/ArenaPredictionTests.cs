using System;
using System.Collections;
using NUnit.Framework;
using UnityEngine;
using UnityEngine.TestTools;

/// <summary>
/// The Godot <c>predict_smoke.gd</c> checks against <c>wrangler dev</c> in
/// <c>examples/arena</c>, with the custom arena plugin. Ignored without <c>HEARTH_URL</c>.
/// </summary>
public class ArenaPredictionTests
{
    [UnityTest, Timeout(60000)]
    public IEnumerator Prediction_MovesAtOnce_AndConvergesAfterCorrections()
    {
        var url = Environment.GetEnvironmentVariable("HEARTH_URL");
        if (string.IsNullOrEmpty(url))
            Assert.Ignore("set HEARTH_URL to the arena gateway (wrangler dev in examples/arena)");
        var run = ArenaScenario.Run(url, Debug.Log);
        while (!run.IsCompleted)
            yield return null;
        Assert.IsNull(run.Result, run.Result);
    }
}
