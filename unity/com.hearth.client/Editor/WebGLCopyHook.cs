using System.IO;
using UnityEditor;
using UnityEditor.Build;
using UnityEditor.Build.Reporting;
using UnityEngine;

namespace Hearth.Editor
{
    /// <summary>
    /// WebGL builds load the wasm core from <c>StreamingAssets/hearth/</c>. This
    /// copies it there before the build and removes it afterwards, so the project
    /// stays clean. A project's own core (<c>Assets/Plugins/Hearth/WebGL~</c>, from
    /// <c>build-unity-native.sh --crate .. --out Assets/Plugins/Hearth</c>) wins over
    /// the package's stock one in <c>Runtime/StreamingAssets~</c>.
    /// </summary>
    internal sealed class WebGLCopyHook : IPreprocessBuildWithReport, IPostprocessBuildWithReport
    {
        static readonly string[] Files = { "hearth_core.js", "hearth_core_bg.wasm" };
        const string Dest = "Assets/StreamingAssets/hearth";

        public int callbackOrder => 0;

        const string ProjectCore = "Assets/Plugins/Hearth/WebGL~";

        static string Source => Directory.Exists(ProjectCore)
            ? Path.GetFullPath(ProjectCore)
            : Path.Combine(Path.GetFullPath("Packages/com.hearth.client"), "Runtime", "StreamingAssets~");

        public void OnPreprocessBuild(BuildReport report)
        {
            if (report.summary.platform != BuildTarget.WebGL)
                return;
            Directory.CreateDirectory(Dest);
            foreach (var file in Files)
            {
                var from = Path.Combine(Source, file);
                if (!File.Exists(from))
                    throw new BuildFailedException(
                        $"Hearth: {from} is missing; run scripts/build-unity-native.sh --only webgl");
                File.Copy(from, Path.Combine(Dest, file), true);
            }
            AssetDatabase.Refresh();
        }

        public void OnPostprocessBuild(BuildReport report)
        {
            if (report.summary.platform != BuildTarget.WebGL)
                return;
            FileUtil.DeleteFileOrDirectory(Dest);
            FileUtil.DeleteFileOrDirectory(Dest + ".meta");
            AssetDatabase.Refresh();
        }
    }
}
