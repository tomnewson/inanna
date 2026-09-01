using System.Security;

namespace Inanna.Services;

internal static class LegacyDataMigration
{
    internal const string CompletionMarkerFileName = ".legacy-data-migration-complete";

    private static readonly HashSet<string> ExcludedTopLevelEntries = new(
        StringComparer.OrdinalIgnoreCase)
    {
        "logs",
        "staging",
        "update.lock",
    };

    public static bool TryMigrate(string legacyRoot, string destinationRoot)
    {
        var completionMarker = Path.Combine(destinationRoot, CompletionMarkerFileName);
        if (File.Exists(completionMarker))
        {
            return true;
        }

        try
        {
            Directory.CreateDirectory(destinationRoot);
            if (Directory.Exists(legacyRoot))
            {
                CopyDirectoryContents(legacyRoot, destinationRoot, isTopLevel: true);
            }
            WriteCompletionMarker(completionMarker);
            return true;
        }
        catch (Exception error) when (IsRecoverableFileSystemError(error))
        {
            // The old application or a security scanner may still have a file open.
            // Keep using the legacy root for this launch and retry on the next one.
            return false;
        }
    }

    private static void CopyDirectoryContents(
        string sourceRoot,
        string destinationRoot,
        bool isTopLevel)
    {
        foreach (var sourceDirectory in Directory.EnumerateDirectories(sourceRoot))
        {
            var name = Path.GetFileName(sourceDirectory);
            if ((isTopLevel && ExcludedTopLevelEntries.Contains(name)) ||
                IsReparsePoint(sourceDirectory))
            {
                continue;
            }

            var destinationDirectory = Path.Combine(destinationRoot, name);
            Directory.CreateDirectory(destinationDirectory);
            CopyDirectoryContents(sourceDirectory, destinationDirectory, isTopLevel: false);
        }

        foreach (var sourceFile in Directory.EnumerateFiles(sourceRoot))
        {
            var name = Path.GetFileName(sourceFile);
            if ((isTopLevel && ExcludedTopLevelEntries.Contains(name)) ||
                IsReparsePoint(sourceFile))
            {
                continue;
            }

            var destinationFile = Path.Combine(destinationRoot, name);
            if (File.Exists(destinationFile) || Directory.Exists(destinationFile))
            {
                continue;
            }
            CopyFileSafely(sourceFile, destinationFile);
        }
    }

    private static void CopyFileSafely(string source, string destination)
    {
        var temporary = $"{destination}.migration-{Guid.NewGuid():N}.tmp";
        try
        {
            using (var input = new FileStream(
                       source,
                       FileMode.Open,
                       FileAccess.Read,
                       FileShare.ReadWrite | FileShare.Delete))
            using (var output = new FileStream(
                       temporary,
                       FileMode.CreateNew,
                       FileAccess.Write,
                       FileShare.None))
            {
                input.CopyTo(output);
                output.Flush(flushToDisk: true);
            }

            if (!OperatingSystem.IsWindows())
            {
                File.SetUnixFileMode(temporary, File.GetUnixFileMode(source));
            }
            File.SetLastWriteTimeUtc(temporary, File.GetLastWriteTimeUtc(source));

            try
            {
                File.Move(temporary, destination);
            }
            catch (IOException) when (File.Exists(destination) || Directory.Exists(destination))
            {
                // Another Inanna process completed the same copy first.
            }
        }
        finally
        {
            TryDeleteTemporaryFile(temporary);
        }
    }

    private static void WriteCompletionMarker(string path)
    {
        try
        {
            using var marker = new FileStream(
                path,
                FileMode.CreateNew,
                FileAccess.Write,
                FileShare.Read);
            marker.Flush(flushToDisk: true);
        }
        catch (IOException) when (File.Exists(path))
        {
            // Another first-launch process completed the migration.
        }
    }

    private static bool IsReparsePoint(string path) =>
        (File.GetAttributes(path) & FileAttributes.ReparsePoint) != 0;

    private static bool IsRecoverableFileSystemError(Exception error) =>
        error is IOException or UnauthorizedAccessException or SecurityException;

    private static void TryDeleteTemporaryFile(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch (Exception error) when (IsRecoverableFileSystemError(error))
        {
        }
    }
}
