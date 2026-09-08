using Velopack;
using Velopack.Sources;

namespace Inanna.Services;

public static class ApplicationUpdater
{
    // Keep the original endpoint so released builds continue to find the same feed.
    // GitHub redirects it if the repository itself is renamed later.
    private const string RepositoryUrl = "https://github.com/tomnewson/yt-dlp-wrapper";
    private const string UpdateCheckTimestampFile = "last-application-update-check.txt";

    public static IApplicationUpdater Create(ApplicationPaths? paths = null)
    {
        try
        {
            paths ??= ApplicationPaths.Create();
            var updater = new VelopackApplicationUpdater(new UpdateManager(
                new GithubSource(RepositoryUrl, accessToken: null, prerelease: false)));
            return new ThrottledApplicationUpdater(
                updater,
                Path.Combine(paths.DataRoot, UpdateCheckTimestampFile));
        }
        catch (Exception)
        {
            return UnavailableApplicationUpdater.Instance;
        }
    }
}

internal sealed class ThrottledApplicationUpdater(
    IApplicationUpdater inner,
    string timestampPath,
    Func<DateTimeOffset>? getUtcNow = null) : IApplicationUpdater
{
    private static readonly TimeSpan MinimumCheckInterval = TimeSpan.FromMinutes(5);
    private readonly Func<DateTimeOffset> _getUtcNow = getUtcNow ?? (() => DateTimeOffset.UtcNow);

    public bool CanUpdate => inner.CanUpdate;

    public async Task<ApplicationUpdate?> CheckForUpdatesAsync(
        CancellationToken cancellationToken = default)
    {
        var now = _getUtcNow();
        if (TryReadLastCheck(out var lastCheck) &&
            lastCheck <= now &&
            now - lastCheck < MinimumCheckInterval)
        {
            return null;
        }

        var directory = Path.GetDirectoryName(timestampPath);
        if (!string.IsNullOrEmpty(directory))
        {
            Directory.CreateDirectory(directory);
        }
        await File.WriteAllTextAsync(
            timestampPath,
            now.ToUnixTimeSeconds().ToString(),
            cancellationToken);
        return await inner.CheckForUpdatesAsync(cancellationToken);
    }

    public Task DownloadAsync(
        ApplicationUpdate update,
        Action<int>? progress = null,
        CancellationToken cancellationToken = default) =>
        inner.DownloadAsync(update, progress, cancellationToken);

    public void ApplyAndRestart(ApplicationUpdate update) => inner.ApplyAndRestart(update);

    private bool TryReadLastCheck(out DateTimeOffset lastCheck)
    {
        try
        {
            var value = File.ReadAllText(timestampPath);
            if (!long.TryParse(value, out var unixSeconds))
            {
                lastCheck = default;
                return false;
            }
            lastCheck = DateTimeOffset.FromUnixTimeSeconds(unixSeconds);
            return true;
        }
        catch (IOException)
        {
            lastCheck = default;
            return false;
        }
        catch (UnauthorizedAccessException)
        {
            lastCheck = default;
            return false;
        }
    }
}

internal sealed class VelopackApplicationUpdater(UpdateManager manager) : IApplicationUpdater
{
    public bool CanUpdate => manager.IsInstalled;

    public async Task<ApplicationUpdate?> CheckForUpdatesAsync(
        CancellationToken cancellationToken = default)
    {
        if (!CanUpdate)
        {
            return null;
        }

        cancellationToken.ThrowIfCancellationRequested();
        var update = await manager.CheckForUpdatesAsync();
        cancellationToken.ThrowIfCancellationRequested();
        return update is null
            ? null
            : new ApplicationUpdate(update.TargetFullRelease.Version.ToString(), update);
    }

    public Task DownloadAsync(
        ApplicationUpdate update,
        Action<int>? progress = null,
        CancellationToken cancellationToken = default) =>
        manager.DownloadUpdatesAsync(GetNativeUpdate(update), progress, cancellationToken);

    public void ApplyAndRestart(ApplicationUpdate update) =>
        manager.ApplyUpdatesAndRestart(GetNativeUpdate(update));

    private static UpdateInfo GetNativeUpdate(ApplicationUpdate update) =>
        update.NativeUpdate as UpdateInfo ??
        throw new ArgumentException("The application update did not originate from Velopack.", nameof(update));
}

internal sealed class UnavailableApplicationUpdater : IApplicationUpdater
{
    public static UnavailableApplicationUpdater Instance { get; } = new();

    public bool CanUpdate => false;

    public Task<ApplicationUpdate?> CheckForUpdatesAsync(CancellationToken cancellationToken = default) =>
        Task.FromResult<ApplicationUpdate?>(null);

    public Task DownloadAsync(
        ApplicationUpdate update,
        Action<int>? progress = null,
        CancellationToken cancellationToken = default) =>
        Task.FromException(new InvalidOperationException("Application updates are unavailable in this build."));

    public void ApplyAndRestart(ApplicationUpdate update) =>
        throw new InvalidOperationException("Application updates are unavailable in this build.");
}
