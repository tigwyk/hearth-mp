using System;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Text;
using NUnit.Framework;
using UnityEngine;

namespace Hearth.Tests
{
    /// <summary>
    /// The C# public surface (<c>Hearth.Client</c>, editor build) against
    /// <c>api/unity-csharp.txt</c>, which <c>spec/stability.md</c> puts under semver.
    /// A change fails until the snapshot is regenerated on purpose:
    /// <c>scripts/check-stability.sh --bless</c> (it sets <c>HEARTH_API_BLESS=1</c>).
    /// On a mismatch the current surface is written to <c>Temp/unity-csharp.txt</c>.
    /// </summary>
    public class ApiSnapshotTests
    {
        const BindingFlags Members = BindingFlags.Public | BindingFlags.Instance | BindingFlags.Static | BindingFlags.DeclaredOnly;

        static string Name(Type t)
        {
            if (t.IsGenericParameter)
                return t.Name;
            if (t.IsArray)
                return Name(t.GetElementType()) + "[]";
            if (t.IsByRef)
                return Name(t.GetElementType()) + "&";
            var name = t.IsGenericType ? t.Name.Substring(0, t.Name.IndexOf('`')) : t.Name;
            var full = (t.IsNested ? Name(t.DeclaringType) + "." : t.Namespace + ".") + name;
            return t.IsGenericType ? $"{full}<{string.Join(", ", t.GetGenericArguments().Select(Name))}>" : full;
        }

        static string Params(MethodBase m) =>
            string.Join(", ", m.GetParameters().Select(p => $"{Name(p.ParameterType)} {p.Name}{(p.HasDefaultValue ? " = " + (p.DefaultValue ?? "null") : "")}"));

        internal static string Surface()
        {
            var lines = new StringBuilder();
            foreach (var type in typeof(HearthClient).Assembly.GetExportedTypes().OrderBy(Name, StringComparer.Ordinal))
            {
                var kind = type.IsEnum ? "enum" : type.IsInterface ? "interface" : type.IsValueType ? "struct"
                    : typeof(Delegate).IsAssignableFrom(type) ? "delegate" : (type.IsAbstract && type.IsSealed) ? "static class"
                    : type.IsAbstract ? "abstract class" : type.IsSealed ? "sealed class" : "class";
                var bases = new[] { type.BaseType }.Where(b => b != null && b != typeof(object) && b != typeof(ValueType) && b != typeof(Enum))
                    .Concat(type.GetInterfaces()).Select(Name).OrderBy(n => n, StringComparer.Ordinal);
                var header = $"{kind} {Name(type)}";
                if (bases.Any())
                    header += " : " + string.Join(", ", bases);
                var members = new System.Collections.Generic.List<string>();
                if (type.IsEnum)
                {
                    members.AddRange(Enum.GetNames(type).Select(n => $"  {n}"));
                }
                else
                {
                    foreach (var c in type.GetConstructors(Members))
                        members.Add($"  ctor({Params(c)})");
                    foreach (var f in type.GetFields(Members))
                        members.Add($"  field {(f.IsStatic ? "static " : "")}{(f.IsLiteral ? "const " : "")}{Name(f.FieldType)} {f.Name}");
                    foreach (var p in type.GetProperties(Members))
                    {
                        var get = p.GetMethod != null && p.GetMethod.IsPublic ? " get;" : "";
                        var set = p.SetMethod != null && p.SetMethod.IsPublic ? " set;" : "";
                        var index = p.GetIndexParameters();
                        var name = index.Length == 0 ? p.Name : $"this[{string.Join(", ", index.Select(i => Name(i.ParameterType)))}]";
                        members.Add($"  property {Name(p.PropertyType)} {name} {{{get}{set} }}");
                    }
                    foreach (var e in type.GetEvents(Members))
                        members.Add($"  event {Name(e.EventHandlerType)} {e.Name}");
                    foreach (var m in type.GetMethods(Members).Where(m => !m.IsSpecialName))
                    {
                        var generic = m.IsGenericMethod ? $"<{string.Join(", ", m.GetGenericArguments().Select(Name))}>" : "";
                        members.Add($"  method {(m.IsStatic ? "static " : "")}{Name(m.ReturnType)} {m.Name}{generic}({Params(m)})");
                    }
                }
                lines.AppendLine(header);
                foreach (var member in members.OrderBy(m => m, StringComparer.Ordinal))
                    lines.AppendLine(member);
            }
            return lines.ToString().Replace("\r\n", "\n");
        }

        static string RepoFile(string relative)
        {
            for (var dir = new DirectoryInfo(Application.dataPath); dir != null; dir = dir.Parent)
                if (Directory.Exists(Path.Combine(dir.FullName, "conformance")))
                    return Path.Combine(dir.FullName, relative);
            Assert.Fail("the repository root (with conformance/) was not found above " + Application.dataPath);
            return null;
        }

        [Test]
        public void PublicSurface_MatchesSnapshot()
        {
            var current = Surface();
            var path = RepoFile(Path.Combine("api", "unity-csharp.txt"));
            if (Environment.GetEnvironmentVariable("HEARTH_API_BLESS") == "1")
            {
                File.WriteAllText(path, current);
                return;
            }
            var expected = File.Exists(path) ? File.ReadAllText(path).Replace("\r\n", "\n") : "";
            if (current != expected)
            {
                Directory.CreateDirectory("Temp");
                File.WriteAllText(Path.Combine("Temp", "unity-csharp.txt"), current);
                Assert.Fail("API changed: review, then run scripts/check-stability.sh --bless "
                    + $"(current surface written to {Path.GetFullPath(Path.Combine("Temp", "unity-csharp.txt"))})");
            }
        }
    }
}
