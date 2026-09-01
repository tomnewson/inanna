namespace Inanna.Services;

public sealed record ApplicationPaths(string DataRoot)
{
    private const string DataDirectoryName = "Inanna";
    private const string LegacyDataDirectoryName = "YT-DLP Wrapper";
    private const string DataRootEnvironmentVariable = "INANNA_DATA_ROOT";
    private const string LegacyDataRootEnvironmentVariable = "YT_DLP_WRAPPER_DATA_ROOT";

    public static ApplicationPaths Create()
    {
        var overrideRoot = Environment.GetEnvironmentVariable(DataRootEnvironmentVariable) ??
            Environment.GetEnvironmentVariable(LegacyDataRootEnvironmentVariable);
        if (!string.IsNullOrWhiteSpace(overrideRoot))
        {
            return new ApplicationPaths(Path.GetFullPath(overrideRoot));
        }

        var platformDataDirectory = GetPlatformDataDirectory();
        var dataRoot = Path.Combine(platformDataDirectory, DataDirectoryName);
        var legacyDataRoot = Path.Combine(platformDataDirectory, LegacyDataDirectoryName);
        var migrationCompleted = LegacyDataMigration.TryMigrate(legacyDataRoot, dataRoot);
        return new ApplicationPaths(migrationCompleted ? dataRoot : legacyDataRoot);
    }

    internal static string GetPlatformDataDirectory()
    {
        var folder = OperatingSystem.IsMacOS()
            ? Environment.SpecialFolder.ApplicationData
            : Environment.SpecialFolder.LocalApplicationData;
        var path = Environment.GetFolderPath(folder);
        if (string.IsNullOrWhiteSpace(path))
        {
            throw new InvalidOperationException("The operating system did not provide an application data directory.");
        }

        return path;
    }
}
