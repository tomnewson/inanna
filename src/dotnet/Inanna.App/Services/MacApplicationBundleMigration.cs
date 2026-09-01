using System.ComponentModel;
using System.Diagnostics;
using System.Security;

namespace Inanna.Services;

internal static class MacApplicationBundleMigration
{
    internal const string LegacyBundleName = "YT-DLP Wrapper.app";
    internal const string CurrentBundleName = "Inanna.app";
    private const string ExecutableName = "yt-dlp-wrapper";

    public static string? TryRenameCurrentBundle()
    {
        if (!OperatingSystem.IsMacOS())
        {
            return null;
        }

        var legacyBundle = FindLegacyBundleRoot(AppContext.BaseDirectory);
        return legacyBundle is null ? null : TryRenameBundle(legacyBundle);
    }

    internal static string? FindLegacyBundleRoot(string path)
    {
        for (var directory = new DirectoryInfo(Path.GetFullPath(path));
             directory is not null;
             directory = directory.Parent)
        {
            if (!directory.Name.EndsWith(".app", StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            return string.Equals(
                directory.Name,
                LegacyBundleName,
                StringComparison.OrdinalIgnoreCase)
                ? directory.FullName
                : null;
        }

        return null;
    }

    internal static string? TryRenameBundle(string legacyBundle)
    {
        var source = Path.GetFullPath(legacyBundle);
        if (!string.Equals(
                Path.GetFileName(source),
                LegacyBundleName,
                StringComparison.OrdinalIgnoreCase))
        {
            return null;
        }

        var parent = Directory.GetParent(source);
        if (parent is null)
        {
            return null;
        }

        var destination = Path.Combine(parent.FullName, CurrentBundleName);
        var destinationExecutable = Path.Combine(
            destination,
            "Contents",
            "MacOS",
            ExecutableName);
        if (!Directory.Exists(source))
        {
            // A concurrently launched instance may have completed the atomic rename.
            return File.Exists(destinationExecutable) ? destinationExecutable : null;
        }

        var sourceExecutable = Path.Combine(source, "Contents", "MacOS", ExecutableName);
        if (!File.Exists(sourceExecutable))
        {
            return null;
        }

        if (Directory.Exists(destination) || File.Exists(destination))
        {
            // Never replace a separately installed Inanna bundle.
            return null;
        }

        try
        {
            // A directory rename is atomic on the same volume. It does not copy,
            // remove, or otherwise touch any files currently open by this process.
            Directory.Move(source, destination);
            return destinationExecutable;
        }
        catch (Exception error) when (IsRecoverableFileSystemError(error))
        {
            if (!Directory.Exists(source) && File.Exists(destinationExecutable))
            {
                return destinationExecutable;
            }

            // A read-only or managed Applications directory should not prevent the
            // existing installation from launching or receiving future updates.
            return null;
        }
    }

    public static bool TryStartRenamedApplication(string executable)
    {
        try
        {
            return Process.Start(CreateRestartStartInfo(executable)) is not null;
        }
        catch (Exception error) when (
            error is InvalidOperationException or IOException or UnauthorizedAccessException or
                Win32Exception or SecurityException)
        {
            // The bundle was renamed successfully and can be opened normally from
            // Finder even if the automatic relaunch is blocked by the operating system.
            return false;
        }
    }

    internal static ProcessStartInfo CreateRestartStartInfo(string executable)
    {
        var startInfo = new ProcessStartInfo
        {
            FileName = executable,
            WorkingDirectory = Path.GetDirectoryName(executable)!,
            UseShellExecute = false,
        };

        foreach (var name in startInfo.Environment.Keys
                     .Where(name => name.StartsWith("VELOPACK_", StringComparison.OrdinalIgnoreCase))
                     .ToArray())
        {
            startInfo.Environment.Remove(name);
        }

        return startInfo;
    }

    private static bool IsRecoverableFileSystemError(Exception error) =>
        error is IOException or UnauthorizedAccessException or SecurityException;
}
