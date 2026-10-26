using System.IO;
using Hearth;
using UnityEditor;
using UnityEditor.Build;
using UnityEditor.SceneManagement;
using UnityEngine;

/// <summary>Batch-mode entry points: <c>Unity -batchmode -projectPath . -executeMethod BuildScript.&lt;Name&gt;</c>.</summary>
public static class BuildScript
{
    const string Scene = "Assets/Scenes/Main.unity";

    /// <summary>
    /// Create the scene and turn on <c>HEARTH_CUSTOM_PLUGIN</c>, which excludes the
    /// package's stock <c>hearth_unity</c> binaries so this project's own plugin
    /// (in <c>Assets/Plugins/Hearth</c>) is the only one.
    /// </summary>
    public static void Setup()
    {
        foreach (var target in new[] { NamedBuildTarget.Standalone, NamedBuildTarget.WebGL, NamedBuildTarget.Android, NamedBuildTarget.iOS })
            PlayerSettings.SetScriptingDefineSymbols(target, "HEARTH_CUSTOM_PLUGIN");
        Directory.CreateDirectory("Assets/Scenes");
        var scene = EditorSceneManager.NewScene(NewSceneSetup.DefaultGameObjects, NewSceneMode.Single);
        var go = new GameObject("Arena");
        go.AddComponent<HearthClient>();
        go.AddComponent<Arena>();
        EditorSceneManager.SaveScene(scene, Scene);
        EditorBuildSettings.scenes = new[] { new EditorBuildSettingsScene(Scene, true) };
    }

    public static void WebGL() => Build(BuildTarget.WebGL, "Build/WebGL", ScriptingImplementation.IL2CPP);

    /// <summary>IL2CPP + High stripping, development (the smoke run needs DebugLatencyMs).</summary>
    public static void StandaloneIL2CPP() => Stripped(ScriptingImplementation.IL2CPP, "Build/Win64/Arena.exe");

    /// <summary>Mono + High stripping, for editors without the IL2CPP module.</summary>
    public static void StandaloneMonoStripped() => Stripped(ScriptingImplementation.Mono2x, "Build/Win64Mono/Arena.exe");

    static void Stripped(ScriptingImplementation backend, string path)
    {
        PlayerSettings.SetManagedStrippingLevel(NamedBuildTarget.Standalone, ManagedStrippingLevel.High);
        Build(BuildTarget.StandaloneWindows64, path, backend, BuildOptions.Development);
    }

    static void Build(BuildTarget target, string path, ScriptingImplementation backend, BuildOptions options = BuildOptions.None)
    {
        var group = BuildPipeline.GetBuildTargetGroup(target);
        PlayerSettings.SetScriptingBackend(NamedBuildTarget.FromBuildTargetGroup(group), backend);
        var report = BuildPipeline.BuildPlayer(new[] { Scene }, path, target, options);
        if (report.summary.result != UnityEditor.Build.Reporting.BuildResult.Succeeded)
            EditorApplication.Exit(1);
    }
}
