using System.IO;
using Newtonsoft.Json.Linq;
using NUnit.Framework;
using UnityEngine;

namespace Hearth.Tests
{
    /// <summary>
    /// Loads <c>conformance/vectors/&lt;name&gt;</c> by walking up from the project to
    /// the repository. A missing file or zero cases fails the test with the
    /// resolved path, so a moved file can never turn into a silent pass.
    /// </summary>
    internal static class Vectors
    {
        internal static JArray Load(string name)
        {
            var dir = new DirectoryInfo(Application.dataPath);
            string path = null;
            for (; dir != null; dir = dir.Parent)
            {
                var candidate = Path.Combine(dir.FullName, "conformance", "vectors", name);
                if (File.Exists(candidate))
                {
                    path = candidate;
                    break;
                }
            }
            if (path == null)
                Assert.Fail($"conformance/vectors/{name} not found above {Application.dataPath}");
            var cases = JArray.Parse(File.ReadAllText(path));
            if (cases.Count == 0)
                Assert.Fail($"{path} has no cases");
            TestContext.WriteLine($"{name}: {cases.Count} cases");
            return cases;
        }
    }
}
