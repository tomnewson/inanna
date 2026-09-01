namespace Inanna.Services;

public sealed record ApplicationUpdate(string Version, object NativeUpdate);

public interface IApplicationUpdater
{
    bool CanUpdate { get; }
    string CurrentVersion { get; }
    Task<ApplicationUpdate?> CheckForUpdatesAsync(CancellationToken cancellationToken = default);
    Task DownloadAsync(
        ApplicationUpdate update,
        Action<int>? progress = null,
        CancellationToken cancellationToken = default);
    void ApplyAndRestart(ApplicationUpdate update);
}
