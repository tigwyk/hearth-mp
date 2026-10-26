using System.IO;
using Hearth;
using UnityEditor;
using UnityEditor.SceneManagement;
using UnityEngine;

/// <summary>Batch-mode entry points: <c>Unity -batchmode -projectPath . -executeMethod BuildScript.&lt;Name&gt;</c>.</summary>
public static class BuildScript
{
    const string Scene = "Assets/Scenes/Main.unity";

    /// <summary>Create the sample scene: one GameObject with HearthClient + Board.</summary>
    public static void CreateScene()
    {
        Directory.CreateDirectory("Assets/Scenes");
        var scene = EditorSceneManager.NewScene(NewSceneSetup.DefaultGameObjects, NewSceneMode.Single);
        var go = new GameObject("Hearth");
        go.AddComponent<HearthClient>();
        go.AddComponent<Board>();
        EditorSceneManager.SaveScene(scene, Scene);
        EditorBuildSettings.scenes = new[] { new EditorBuildSettingsScene(Scene, true) };
    }

    public static void WebGL() => Build(BuildTarget.WebGL, "Build/WebGL", ScriptingImplementation.IL2CPP);

    /// <summary>IL2CPP + High stripping (plan Review Focus 5); run the player with <c>-hearthSmoke</c>.</summary>
    public static void StandaloneIL2CPP()
    {
        PlayerSettings.SetManagedStrippingLevel(UnityEditor.Build.NamedBuildTarget.Standalone, ManagedStrippingLevel.High);
        Build(BuildTarget.StandaloneWindows64, "Build/Win64/TicTacToe.exe", ScriptingImplementation.IL2CPP);
    }

    /// <summary>Mono + High stripping, for editors without the IL2CPP module.</summary>
    public static void StandaloneMonoStripped()
    {
        PlayerSettings.SetManagedStrippingLevel(UnityEditor.Build.NamedBuildTarget.Standalone, ManagedStrippingLevel.High);
        Build(BuildTarget.StandaloneWindows64, "Build/Win64Mono/TicTacToe.exe", ScriptingImplementation.Mono2x);
    }

    static void Build(BuildTarget target, string path, ScriptingImplementation backend)
    {
        var group = BuildPipeline.GetBuildTargetGroup(target);
        PlayerSettings.SetScriptingBackend(UnityEditor.Build.NamedBuildTarget.FromBuildTargetGroup(group), backend);
        var report = BuildPipeline.BuildPlayer(new[] { Scene }, path, target, BuildOptions.None);
        if (report.summary.result != UnityEditor.Build.Reporting.BuildResult.Succeeded)
            EditorApplication.Exit(1);
    }
}
