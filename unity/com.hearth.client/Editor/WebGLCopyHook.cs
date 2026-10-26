using System.IO;
using UnityEditor;
using UnityEditor.Build;
using UnityEditor.Build.Reporting;
using UnityEngine;

namespace Hearth.Editor
{
    /// <summary>
    /// WebGL builds load the wasm core from <c>StreamingAssets/hearth/</c>. This
    /// copies the package's <c>Runtime/StreamingAssets~</c> files there before the
    /// build and removes them afterwards, so the project stays clean.
    /// </summary>
    internal sealed class WebGLCopyHook : IPreprocessBuildWithReport, IPostprocessBuildWithReport
    {
        static readonly string[] Files = { "hearth_core.js", "hearth_core_bg.wasm" };
        const string Dest = "Assets/StreamingAssets/hearth";

        public int callbackOrder => 0;

        static string Source =>
            Path.Combine(Path.GetFullPath("Packages/com.hearth.client"), "Runtime", "StreamingAssets~");

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
