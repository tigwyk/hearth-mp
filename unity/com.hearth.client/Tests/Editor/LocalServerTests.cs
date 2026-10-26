using Hearth.Editor;
using NUnit.Framework;
using UnityEditor;

namespace Hearth.Tests
{
    public class LocalServerTests
    {
        [Test]
        public void BuildCommand_PrefersHearth() =>
            Assert.AreEqual("hearth-mp dev --port 9000", LocalServer.BuildCommand("../tictactoe", 9000, true));

        [Test]
        public void BuildCommand_FallsBackToWrangler() =>
            Assert.AreEqual("wrangler dev --port 8787", LocalServer.BuildCommand("../tictactoe", 8787, false));

        [Test]
        public void IsReady_AcceptsSchemaJson() =>
            Assert.IsTrue(LocalServer.IsReady("{\"hearth_schema\":1,\"room\":\"tictactoe\"}"));

        [Test]
        public void IsReady_RejectsHtmlErrorPage()
        {
            Assert.IsFalse(LocalServer.IsReady("<!DOCTYPE html><html><body>502</body></html>"));
            Assert.IsFalse(LocalServer.IsReady(""));
        }

        [Test]
        public void UrlOverride_ClearedOnExitPlayMode()
        {
            HearthClient.UrlOverride = "http://127.0.0.1:8787";
            LocalServer.OnPlayModeChanged(PlayModeStateChange.ExitingPlayMode);
            Assert.IsNull(HearthClient.UrlOverride);
        }

        [Test]
        public void DoesNotKillForeignProcess()
        {
            // A server that already answered before play mode is never recorded as ours.
            LocalServer.Started = null;
            Assert.DoesNotThrow(LocalServer.Stop);
            Assert.IsNull(LocalServer.Started);
        }
    }
}
