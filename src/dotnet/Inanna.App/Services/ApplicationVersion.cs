using System.Reflection;

namespace Inanna.Services;

public static class ApplicationVersion
{
    public static string Current { get; } = GetCurrent();

    private static string GetCurrent()
    {
        var assembly = typeof(ApplicationVersion).Assembly;
        var informational = assembly
            .GetCustomAttribute<AssemblyInformationalVersionAttribute>()?
            .InformationalVersion;
        if (!string.IsNullOrWhiteSpace(informational))
        {
            return informational.Split('+', 2)[0];
        }

        return assembly.GetName().Version?.ToString(3) ?? "unknown";
    }
}
