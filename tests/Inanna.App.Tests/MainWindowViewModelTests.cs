using System.Diagnostics;
using System.Text.Json;
using Xunit;
using Inanna.Services;
using Inanna.ViewModels;

namespace Inanna.App.Tests;

public sealed class MainWindowViewModelTests
{
    [Fact]
    public async Task InitializeLoadsFolderAndReadyState()
    {
        var backend = new FakeBackendClient();
        backend.Enqueue("initialize", InitializeResult());
        backend.Enqueue("checkTools", Json(
            """{"state":"ready","toolsReady":true,"canInstallTools":false,"updateSummary":"","statusText":"Ready."}"""));
        var viewModel = CreateViewModel(backend);

        await viewModel.InitializeAsync();

        Assert.Equal("C:/Videos", viewModel.OutputFolder);
        Assert.True(viewModel.ToolsReady);
        Assert.False(viewModel.Busy);
        Assert.Equal("Ready.", viewModel.StatusText);
    }

    [Fact]
    public async Task ToolUpdateDoesNotBlockDownload()
    {
        var backend = new FakeBackendClient();
        backend.Enqueue("initialize", InitializeResult());
        backend.Enqueue("checkTools", Json(
            """{"state":"updateAvailable","toolsReady":true,"canInstallTools":true,"updateSummary":"yt-dlp 1","statusText":"Updates are available."}"""));
        var viewModel = CreateViewModel(backend);
        viewModel.Url = "https://example.com/video";

        await viewModel.InitializeAsync();
        Assert.True(viewModel.CanDownload);

        viewModel.DeferUpdateCommand.Execute(null);
        Assert.True(viewModel.CanDownload);
        Assert.Equal("Ready. The cached tools will be used.", viewModel.StatusText);
    }

    [Fact]
    public async Task DownloadCompletionMakesFileRevealAvailable()
    {
        var backend = new FakeBackendClient();
        var platform = new FakePlatformServices();
        backend.Enqueue("startDownload", Json("""{"operationId":"operation-1"}"""));
        var viewModel = CreateViewModel(backend, platform);
        viewModel.Url = "https://example.com/video";
        viewModel.OutputFolder = "C:/Videos";
        viewModel.ToolsReady = true;
        viewModel.Busy = false;

        await viewModel.StartDownloadCommand.ExecuteAsync(null);
        backend.Raise(new BackendEvent(
            "operation-1",
            "operationCompleted",
            Json("""{"operationKind":"download","path":"C:/Videos/café.mp4"}""")));

        Assert.True(viewModel.Completed);
        Assert.True(viewModel.HasCompletedFile);
        Assert.Equal(100, viewModel.Progress);
        Assert.Equal("Saved C:/Videos/café.mp4", viewModel.StatusText);

        viewModel.OpenFolderCommand.Execute(null);
        Assert.Equal("C:/Videos/café.mp4", platform.RevealedPath);
    }

    [Theory]
    [InlineData(0, "audioOnly", "best")]
    [InlineData(1, "video", "p1080")]
    [InlineData(2, "video", "p1440")]
    [InlineData(3, "video", "best")]
    public async Task QualitySliderSelectsDownloadMode(
        double sliderValue,
        string expectedMode,
        string expectedVideoQuality)
    {
        var backend = new FakeBackendClient();
        backend.Enqueue("startDownload", Json("""{"operationId":"operation-1"}"""));
        var viewModel = CreateViewModel(backend);
        viewModel.Url = "https://example.com/video";
        viewModel.OutputFolder = "C:/Videos";
        viewModel.ToolsReady = true;
        viewModel.Busy = false;
        viewModel.VideoQuality = sliderValue;

        await viewModel.StartDownloadCommand.ExecuteAsync(null);

        Assert.Equal(expectedMode, backend.LastParameters.GetProperty("mode").GetString());
        Assert.Equal(expectedVideoQuality, backend.LastParameters.GetProperty("videoQuality").GetString());
    }

    [Fact]
    public async Task CancelledUpdateKeepsPreviouslyCachedToolsReady()
    {
        var backend = new FakeBackendClient();
        backend.Enqueue("installTools", Json("""{"operationId":"operation-2"}"""));
        var viewModel = CreateViewModel(backend);
        viewModel.ToolsReady = true;
        viewModel.UpdateAvailable = true;
        viewModel.CanInstallTools = true;
        viewModel.Busy = false;

        await viewModel.InstallToolsCommand.ExecuteAsync(null);
        backend.Raise(new BackendEvent(
            "operation-2",
            "operationCancelled",
            Json("""{"operationKind":"toolInstall"}""")));

        Assert.True(viewModel.ToolsReady);
        Assert.False(viewModel.SetupRequired);
        Assert.True(viewModel.CanInstallTools);
    }

    [Fact]
    public async Task CompletedToolInstallClearsStatus()
    {
        var backend = new FakeBackendClient();
        backend.Enqueue("installTools", Json("""{"operationId":"operation-2"}"""));
        var viewModel = CreateViewModel(backend);
        viewModel.UpdateAvailable = true;
        viewModel.CanInstallTools = true;
        viewModel.Busy = false;

        await viewModel.InstallToolsCommand.ExecuteAsync(null);
        backend.Raise(new BackendEvent(
            "operation-2",
            "operationCompleted",
            Json("""{"operationKind":"toolInstall"}""")));

        Assert.True(viewModel.ToolsReady);
        Assert.False(viewModel.ShowStatusText);
        Assert.Equal(string.Empty, viewModel.StatusText);
    }

    [Fact]
    public async Task FailedStartupCanBeRetriedExplicitly()
    {
        var backend = new FakeBackendClient();
        var viewModel = CreateViewModel(backend);

        await viewModel.InitializeAsync();
        Assert.True(viewModel.ShowRestartButton);

        backend.Enqueue("initialize", InitializeResult());
        backend.Enqueue("checkTools", Json(
            """{"state":"ready","toolsReady":true,"canInstallTools":false,"updateSummary":"","statusText":"Ready."}"""));
        await viewModel.RestartBackendCommand.ExecuteAsync(null);

        Assert.False(viewModel.EngineUnavailable);
        Assert.True(viewModel.ToolsReady);
    }

    [Fact]
    public void BackendExitDisablesOperations()
    {
        var backend = new FakeBackendClient();
        var viewModel = CreateViewModel(backend);
        viewModel.ToolsReady = true;
        viewModel.Busy = false;

        backend.Exit("Backend failed.");

        Assert.True(viewModel.EngineUnavailable);
        Assert.False(viewModel.ToolsReady);
        Assert.False(viewModel.CanDownload);
        Assert.Equal("Backend failed.", viewModel.DetailsText);
    }

    [Fact]
    public void OnlyIdleReadyStatusIsHidden()
    {
        var viewModel = CreateViewModel(new FakeBackendClient());

        viewModel.StatusText = "Ready.";
        Assert.False(viewModel.ShowStatusText);

        viewModel.StatusText = string.Empty;
        Assert.False(viewModel.ShowStatusText);

        viewModel.StatusText = "Could not check for updates. Cached tools are ready.";
        Assert.True(viewModel.ShowStatusText);
    }

    [Fact]
    public async Task ApplicationUpdateDownloadsStopsBackendAndApplies()
    {
        var backend = new FakeBackendClient();
        var updater = new FakeApplicationUpdater
        {
            NextUpdate = new ApplicationUpdate("0.2.0", new object()),
        };
        backend.Enqueue("initialize", InitializeResult());
        backend.Enqueue("checkTools", Json(
            """{"state":"ready","toolsReady":true,"canInstallTools":false,"updateSummary":"","statusText":"Ready."}"""));
        var viewModel = CreateViewModel(backend, updater: updater);

        await viewModel.InitializeAsync();

        Assert.True(viewModel.ApplicationUpdateAvailable);
        Assert.Equal("Version 0.2.0 is available.", viewModel.ApplicationUpdateStatus);

        await viewModel.InstallApplicationUpdateCommand.ExecuteAsync(null);

        Assert.True(updater.Downloaded);
        Assert.True(updater.Applied);
        Assert.Equal(1, backend.StopCount);
        Assert.Equal(100, viewModel.ApplicationUpdateProgress);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task CachedToolsAllowDownloadWhileBothUpdateChecksArePending(bool backendExits)
    {
        var toolCheck = new TaskCompletionSource<JsonElement>();
        var appCheck = new TaskCompletionSource<ApplicationUpdate?>();
        var backend = new FakeBackendClient();
        backend.Enqueue("initialize", InitializeResult());
        backend.Enqueue("checkTools", toolCheck.Task);
        backend.Enqueue("startDownload", Json("""{"operationId":"download-1"}"""));
        var updater = new FakeApplicationUpdater { CheckResult = appCheck.Task };
        var viewModel = CreateViewModel(backend, updater: updater);
        viewModel.Url = "https://example.com/video";

        var initializing = viewModel.InitializeAsync();

        Assert.False(initializing.IsCompleted);
        Assert.Equal(1, updater.CheckCount);
        Assert.True(viewModel.CanDownload);
        Assert.False(viewModel.ApplicationUpdateBusy);
        await viewModel.StartDownloadCommand.ExecuteAsync(null);
        if (backendExits)
        {
            backend.Exit("Connection lost.");
        }
        var status = viewModel.StatusText;
        toolCheck.SetResult(Json(
            """{"state":"updateAvailable","toolsReady":true,"canInstallTools":true,"updateSummary":"New tools","statusText":"Updates are available."}"""));
        appCheck.SetResult(new ApplicationUpdate("2.0.0", new object()));
        await initializing;

        Assert.Equal(status, viewModel.StatusText);
        Assert.Equal(!backendExits, viewModel.Busy);
        Assert.Equal(!backendExits, viewModel.ToolsReady);
        Assert.False(viewModel.ShowUpdatePanel);
        Assert.Equal(backendExits, viewModel.CanInstallApplicationUpdate);
        Assert.True(viewModel.ApplicationUpdateAvailable);
        if (!backendExits)
        {
            backend.Raise(new BackendEvent("download-1", "operationCompleted",
                Json("""{"operationKind":"download","path":"clip.mp4"}""")));
            Assert.True(viewModel.ShowUpdatePanel);
            Assert.True(viewModel.CanDownload);
        }
    }

    [Fact]
    public async Task FirstSetupWaitsForToolsButNotForApplicationUpdateCheck()
    {
        var toolCheck = new TaskCompletionSource<JsonElement>();
        var backend = new FakeBackendClient();
        backend.Enqueue("initialize", Json(
            $$"""{"backendVersion":"{{ApplicationVersion.Current}}","outputFolder":"C:/Videos","toolsReady":false}"""));
        backend.Enqueue("checkTools", toolCheck.Task);
        var updater = new FakeApplicationUpdater();
        var viewModel = CreateViewModel(backend, updater: updater);
        viewModel.Url = "https://example.com/video";

        var initializing = viewModel.InitializeAsync();
        Assert.False(viewModel.CanDownload);
        Assert.True(viewModel.Busy);
        Assert.Equal(1, updater.CheckCount);
        toolCheck.SetResult(Json(
            """{"state":"setupRequired","toolsReady":false,"canInstallTools":true,"updateSummary":"Install tools","statusText":""}"""));
        await initializing;

        Assert.True(viewModel.SetupRequired);
        Assert.True(viewModel.CanInstallTools);
        Assert.False(viewModel.CanDownload);
        Assert.False(viewModel.Busy);
    }

    [Fact]
    public async Task StartupOffersApplicationUpdateEvenWhenRustFails()
    {
        var backend = new FakeBackendClient();
        backend.Enqueue("initialize", Task.FromException<JsonElement>(new IOException("Rust exited with code 134.")));
        var updater = new FakeApplicationUpdater { NextUpdate = new ApplicationUpdate("2.0.0", new object()) };
        var viewModel = CreateViewModel(backend, updater: updater);

        await viewModel.InitializeAsync();

        Assert.True(viewModel.EngineUnavailable);
        Assert.True(viewModel.ApplicationUpdateAvailable);
        Assert.True(viewModel.CanInstallApplicationUpdate);
        Assert.Equal(1, updater.CheckCount);
    }

    [Fact]
    public async Task StartupChecksApplicationUpdatesBeforeRustHandshakeCompletes()
    {
        var handshake = new TaskCompletionSource<JsonElement>();
        var backend = new FakeBackendClient();
        backend.Enqueue("initialize", handshake.Task);
        var updater = new FakeApplicationUpdater { NextUpdate = new ApplicationUpdate("2.0.0", new object()) };
        var viewModel = CreateViewModel(backend, updater: updater);

        var initializing = viewModel.InitializeAsync();
        Assert.True(viewModel.ApplicationUpdateAvailable);
        Assert.False(initializing.IsCompleted);
        handshake.SetException(new IOException("Rust failed."));
        await initializing;
    }

    [Fact]
    public async Task PassiveChecksFindUpdatesAndRespectDeferral()
    {
        var updater = new FakeApplicationUpdater();
        var viewModel = CreateViewModel(new FakeBackendClient(), updater: updater);
        viewModel.Busy = false;
        viewModel.EngineUnavailable = true;
        viewModel.StatusText = "Engine unavailable.";
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.False(viewModel.ApplicationUpdateAvailable);

        updater.NextUpdate = new ApplicationUpdate("2.0.0", new object());
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.True(viewModel.CanInstallApplicationUpdate);
        Assert.Equal("Engine unavailable.", viewModel.StatusText);
        viewModel.DeferApplicationUpdateCommand.Execute(null);
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.False(viewModel.ApplicationUpdateAvailable);

        updater.NextUpdate = new ApplicationUpdate("2.0.1", new object());
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.True(viewModel.ApplicationUpdateAvailable);
        Assert.Equal("Version 2.0.1 is available.", viewModel.ApplicationUpdateStatus);
    }

    [Fact]
    public async Task PassiveChecksDoNotOverlapOrRunDuringInstallation()
    {
        var pending = new TaskCompletionSource<ApplicationUpdate?>();
        var updater = new FakeApplicationUpdater { CheckResult = pending.Task };
        var viewModel = CreateViewModel(new FakeBackendClient(), updater: updater);
        var first = viewModel.CheckForApplicationUpdateAsync();
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.Equal(1, updater.CheckCount);
        viewModel.ApplicationUpdateBusy = true;
        pending.SetResult(new ApplicationUpdate("2.0.0", new object()));
        await first;
        Assert.False(viewModel.ApplicationUpdateAvailable);
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.Equal(1, updater.CheckCount);
        viewModel.ApplicationUpdateBusy = false;
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.Equal(2, updater.CheckCount);
        Assert.True(viewModel.ApplicationUpdateAvailable);
    }

    [Fact]
    public async Task PassiveCheckFailureOrThrottlePreservesAnAvailableUpdate()
    {
        var updater = new FakeApplicationUpdater { NextUpdate = new ApplicationUpdate("2.0.0", new object()) };
        var viewModel = CreateViewModel(new FakeBackendClient(), updater: updater);
        await viewModel.CheckForApplicationUpdateAsync();
        updater.NextUpdate = null;
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.True(viewModel.ApplicationUpdateAvailable);
        updater.CheckResult = Task.FromException<ApplicationUpdate?>(new IOException("Offline"));
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.True(viewModel.ApplicationUpdateAvailable);
        Assert.Equal("Version 2.0.0 is available.", viewModel.ApplicationUpdateStatus);
        updater.CheckResult = null;
        updater.NextUpdate = new ApplicationUpdate("2.0.1", new object());
        await viewModel.CheckForApplicationUpdateAsync();
        Assert.Equal("Version 2.0.1 is available.", viewModel.ApplicationUpdateStatus);
    }

    private static JsonElement Json(string value) => JsonSerializer.Deserialize<JsonElement>(value);

    private static JsonElement InitializeResult() => Json(
        $$"""{"backendVersion":"{{ApplicationVersion.Current}}","outputFolder":"C:/Videos","toolsReady":true}""");

    private static MainWindowViewModel CreateViewModel(
        FakeBackendClient backend,
        IPlatformServices? platform = null,
        IApplicationUpdater? updater = null) =>
        new(backend, platform ?? new FakePlatformServices(), action => action(), updater);
}

public sealed class PlatformServicesTests
{
    [Fact]
    public void BackendPipesUseBomlessUtf8()
    {
        var startInfo = PlatformServices.Create(() => null, new ApplicationPaths("test-data")).CreateBackendStartInfo();

        Assert.Equal("utf-8", startInfo.StandardInputEncoding?.WebName);
        Assert.Equal("utf-8", startInfo.StandardOutputEncoding?.WebName);
        Assert.Equal("utf-8", startInfo.StandardErrorEncoding?.WebName);
        Assert.Empty(startInfo.StandardInputEncoding?.GetPreamble() ?? []);
    }

    [Fact]
    public void BackendUsesPersistentApplicationDataRoot()
    {
        var paths = new ApplicationPaths("C:/Users/test/AppData/Local/Inanna");
        var startInfo = PlatformServices.Create(() => null, paths).CreateBackendStartInfo();

        Assert.Equal("--data-root", startInfo.ArgumentList[0]);
        Assert.Equal(paths.DataRoot, startInfo.ArgumentList[1]);
    }

}

public sealed class LegacyDataMigrationTests
{
    [Fact]
    public void CopiesPersistentDataOnceWithoutRemovingLegacyFiles()
    {
        var root = Path.Combine(Path.GetTempPath(), $"inanna-migration-tests-{Guid.NewGuid():N}");
        var legacyRoot = Path.Combine(root, "YT-DLP Wrapper");
        var destinationRoot = Path.Combine(root, "Inanna");
        var legacyTool = Path.Combine(legacyRoot, "tools", "one", "yt-dlp");

        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(legacyTool)!);
            File.WriteAllText(Path.Combine(legacyRoot, "config.json"), "legacy-config");
            File.WriteAllText(legacyTool, "tool");
            Directory.CreateDirectory(Path.Combine(legacyRoot, "logs"));
            File.WriteAllText(Path.Combine(legacyRoot, "logs", "legacy.log"), "log");
            Directory.CreateDirectory(Path.Combine(legacyRoot, "staging"));
            File.WriteAllText(Path.Combine(legacyRoot, "staging", "partial"), "partial");
            File.WriteAllText(Path.Combine(legacyRoot, "update.lock"), string.Empty);

            if (!OperatingSystem.IsWindows())
            {
                File.SetUnixFileMode(
                    legacyTool,
                    UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);
            }

            Assert.True(LegacyDataMigration.TryMigrate(legacyRoot, destinationRoot));

            Assert.Equal(
                "legacy-config",
                File.ReadAllText(Path.Combine(destinationRoot, "config.json")));
            var migratedTool = Path.Combine(destinationRoot, "tools", "one", "yt-dlp");
            Assert.Equal("tool", File.ReadAllText(migratedTool));
            Assert.True(File.Exists(Path.Combine(legacyRoot, "config.json")));
            Assert.True(File.Exists(legacyTool));
            Assert.False(Directory.Exists(Path.Combine(destinationRoot, "logs")));
            Assert.False(Directory.Exists(Path.Combine(destinationRoot, "staging")));
            Assert.False(File.Exists(Path.Combine(destinationRoot, "update.lock")));
            Assert.True(File.Exists(Path.Combine(
                destinationRoot,
                LegacyDataMigration.CompletionMarkerFileName)));
            if (!OperatingSystem.IsWindows())
            {
                Assert.True((File.GetUnixFileMode(migratedTool) & UnixFileMode.UserExecute) != 0);
            }

            File.WriteAllText(Path.Combine(legacyRoot, "created-later.json"), "later");
            Assert.True(LegacyDataMigration.TryMigrate(legacyRoot, destinationRoot));
            Assert.False(File.Exists(Path.Combine(destinationRoot, "created-later.json")));
        }
        finally
        {
            if (Directory.Exists(root))
            {
                Directory.Delete(root, recursive: true);
            }
        }
    }

    [Fact]
    public void PreservesFilesAlreadyCreatedByInanna()
    {
        var root = Path.Combine(Path.GetTempPath(), $"inanna-migration-tests-{Guid.NewGuid():N}");
        var legacyRoot = Path.Combine(root, "YT-DLP Wrapper");
        var destinationRoot = Path.Combine(root, "Inanna");

        try
        {
            Directory.CreateDirectory(legacyRoot);
            Directory.CreateDirectory(destinationRoot);
            File.WriteAllText(Path.Combine(legacyRoot, "config.json"), "legacy-config");
            File.WriteAllText(Path.Combine(destinationRoot, "config.json"), "inanna-config");

            Assert.True(LegacyDataMigration.TryMigrate(legacyRoot, destinationRoot));

            Assert.Equal(
                "inanna-config",
                File.ReadAllText(Path.Combine(destinationRoot, "config.json")));
        }
        finally
        {
            if (Directory.Exists(root))
            {
                Directory.Delete(root, recursive: true);
            }
        }
    }

    [Fact]
    public void LockedLegacyFileDefersMigrationWithoutThrowingOrDeletingSource()
    {
        var root = Path.Combine(Path.GetTempPath(), $"inanna-migration-tests-{Guid.NewGuid():N}");
        var legacyRoot = Path.Combine(root, "YT-DLP Wrapper");
        var destinationRoot = Path.Combine(root, "Inanna");
        var legacyConfig = Path.Combine(legacyRoot, "config.json");

        try
        {
            Directory.CreateDirectory(legacyRoot);
            File.WriteAllText(legacyConfig, "legacy-config");

            using (new FileStream(
                       legacyConfig,
                       FileMode.Open,
                       FileAccess.ReadWrite,
                       FileShare.None))
            {
                Assert.False(LegacyDataMigration.TryMigrate(legacyRoot, destinationRoot));
                Assert.True(File.Exists(legacyConfig));
                Assert.False(File.Exists(Path.Combine(
                    destinationRoot,
                    LegacyDataMigration.CompletionMarkerFileName)));
            }

            Assert.True(LegacyDataMigration.TryMigrate(legacyRoot, destinationRoot));
            Assert.Equal(
                "legacy-config",
                File.ReadAllText(Path.Combine(destinationRoot, "config.json")));
        }
        finally
        {
            if (Directory.Exists(root))
            {
                Directory.Delete(root, recursive: true);
            }
        }
    }
}

public sealed class MacApplicationBundleMigrationTests
{
    [Fact]
    public void RenamesLegacyBundleWithoutChangingItsContents()
    {
        var root = Path.Combine(Path.GetTempPath(), $"inanna-bundle-tests-{Guid.NewGuid():N}");
        var legacyBundle = Path.Combine(root, MacApplicationBundleMigration.LegacyBundleName);
        var executable = Path.Combine(legacyBundle, "Contents", "MacOS", "yt-dlp-wrapper");

        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(executable)!);
            File.WriteAllText(executable, "frontend");

            var renamedExecutable = MacApplicationBundleMigration.TryRenameBundle(legacyBundle);

            Assert.Equal(
                Path.Combine(root, "Inanna.app", "Contents", "MacOS", "yt-dlp-wrapper"),
                renamedExecutable);
            Assert.False(Directory.Exists(legacyBundle));
            Assert.Equal("frontend", File.ReadAllText(renamedExecutable!));
        }
        finally
        {
            if (Directory.Exists(root))
            {
                Directory.Delete(root, recursive: true);
            }
        }
    }

    [Fact]
    public void LeavesBothBundlesUntouchedWhenInannaAlreadyExists()
    {
        var root = Path.Combine(Path.GetTempPath(), $"inanna-bundle-tests-{Guid.NewGuid():N}");
        var legacyBundle = Path.Combine(root, MacApplicationBundleMigration.LegacyBundleName);
        var legacyExecutable = Path.Combine(
            legacyBundle,
            "Contents",
            "MacOS",
            "yt-dlp-wrapper");
        var currentBundle = Path.Combine(root, MacApplicationBundleMigration.CurrentBundleName);

        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(legacyExecutable)!);
            File.WriteAllText(legacyExecutable, "legacy");
            Directory.CreateDirectory(currentBundle);
            File.WriteAllText(Path.Combine(currentBundle, "keep.txt"), "current");

            Assert.Null(MacApplicationBundleMigration.TryRenameBundle(legacyBundle));

            Assert.Equal("legacy", File.ReadAllText(legacyExecutable));
            Assert.Equal("current", File.ReadAllText(Path.Combine(currentBundle, "keep.txt")));
        }
        finally
        {
            if (Directory.Exists(root))
            {
                Directory.Delete(root, recursive: true);
            }
        }
    }

    [Fact]
    public void FindsOnlyTheLegacyApplicationBundle()
    {
        Assert.Equal(
            "/Applications/YT-DLP Wrapper.app",
            MacApplicationBundleMigration.FindLegacyBundleRoot(
                "/Applications/YT-DLP Wrapper.app/Contents/MacOS"));
        Assert.Null(MacApplicationBundleMigration.FindLegacyBundleRoot(
            "/Applications/Inanna.app/Contents/MacOS"));
    }

    [Fact]
    public void UsesBundleRenamedByAnotherStartingInstance()
    {
        var root = Path.Combine(Path.GetTempPath(), $"inanna-bundle-tests-{Guid.NewGuid():N}");
        var legacyBundle = Path.Combine(root, MacApplicationBundleMigration.LegacyBundleName);
        var renamedExecutable = Path.Combine(
            root,
            MacApplicationBundleMigration.CurrentBundleName,
            "Contents",
            "MacOS",
            "yt-dlp-wrapper");

        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(renamedExecutable)!);
            File.WriteAllText(renamedExecutable, "frontend");

            Assert.Equal(
                renamedExecutable,
                MacApplicationBundleMigration.TryRenameBundle(legacyBundle));
        }
        finally
        {
            if (Directory.Exists(root))
            {
                Directory.Delete(root, recursive: true);
            }
        }
    }

    [Fact]
    public void RestartDoesNotPropagateVelopackControlVariables()
    {
        var startInfo = MacApplicationBundleMigration.CreateRestartStartInfo(
            "/Applications/Inanna.app/Contents/MacOS/yt-dlp-wrapper");

        Assert.False(startInfo.UseShellExecute);
        Assert.Equal("/Applications/Inanna.app/Contents/MacOS", startInfo.WorkingDirectory);
        Assert.DoesNotContain(
            startInfo.Environment.Keys,
            name => name.StartsWith("VELOPACK_", StringComparison.OrdinalIgnoreCase));
    }
}

public sealed class ApplicationUpdaterTests
{
    [Fact]
    public async Task UpdateChecksAreThrottledForFiveMinutesAcrossInstances()
    {
        var directory = Path.Combine(Path.GetTempPath(), $"inanna-tests-{Guid.NewGuid():N}");
        var timestampPath = Path.Combine(directory, "last-update-check.txt");
        var now = new DateTimeOffset(2026, 8, 21, 12, 0, 0, TimeSpan.Zero);
        var firstInner = new FakeApplicationUpdater();
        var first = new ThrottledApplicationUpdater(firstInner, timestampPath, () => now);

        try
        {
            await first.CheckForUpdatesAsync();
            Assert.True(long.TryParse(await File.ReadAllTextAsync(timestampPath), out _));
            var reopenedInner = new FakeApplicationUpdater();
            var reopened = new ThrottledApplicationUpdater(reopenedInner, timestampPath, () => now.AddMinutes(4));

            await reopened.CheckForUpdatesAsync();

            Assert.Equal(1, firstInner.CheckCount);
            Assert.Equal(0, reopenedInner.CheckCount);

            var afterInterval = new ThrottledApplicationUpdater(
                reopenedInner,
                timestampPath,
                () => now.AddMinutes(5));
            await afterInterval.CheckForUpdatesAsync();

            Assert.Equal(1, reopenedInner.CheckCount);
        }
        finally
        {
            Directory.Delete(directory, recursive: true);
        }
    }
}

internal sealed class FakeBackendClient : IBackendClient
{
    private readonly Dictionary<string, Queue<Task<JsonElement>>> _responses = [];

    public event Action<BackendEvent>? EventReceived;
    public event Action<string?>? BackendExited;
    public int StopCount { get; private set; }
    public JsonElement LastParameters { get; private set; }

    public void Enqueue(string method, JsonElement response) => Enqueue(method, Task.FromResult(response));

    public void Enqueue(string method, Task<JsonElement> response)
    {
        if (!_responses.TryGetValue(method, out var values))
        {
            values = new Queue<Task<JsonElement>>();
            _responses[method] = values;
        }
        values.Enqueue(response);
    }

    public Task StartAsync(CancellationToken cancellationToken = default) => Task.CompletedTask;

    public Task<JsonElement> SendAsync(
        string method,
        object? parameters = null,
        CancellationToken cancellationToken = default)
    {
        LastParameters = JsonSerializer.SerializeToElement(parameters);
        return _responses[method].Dequeue();
    }

    public Task StopAsync()
    {
        StopCount++;
        return Task.CompletedTask;
    }

    public ValueTask DisposeAsync() => ValueTask.CompletedTask;

    public void Raise(BackendEvent message) => EventReceived?.Invoke(message);
    public void Exit(string? message = null) => BackendExited?.Invoke(message);
}

internal sealed class FakeApplicationUpdater : IApplicationUpdater
{
    public ApplicationUpdate? NextUpdate { get; set; }
    public Task<ApplicationUpdate?>? CheckResult { get; set; }
    public bool Downloaded { get; private set; }
    public bool Applied { get; private set; }
    public int CheckCount { get; private set; }
    public bool CanUpdate => true;

    public Task<ApplicationUpdate?> CheckForUpdatesAsync(CancellationToken cancellationToken = default)
    {
        CheckCount++;
        return CheckResult ?? Task.FromResult(NextUpdate);
    }

    public Task DownloadAsync(
        ApplicationUpdate update,
        Action<int>? progress = null,
        CancellationToken cancellationToken = default)
    {
        Downloaded = true;
        progress?.Invoke(100);
        return Task.CompletedTask;
    }

    public void ApplyAndRestart(ApplicationUpdate update) => Applied = true;
}

internal sealed class FakePlatformServices : IPlatformServices
{
    public string? RevealedPath { get; private set; }

    public ProcessStartInfo CreateBackendStartInfo() => new("backend");
    public Task<string?> PickOutputFolderAsync(string? currentFolder) => Task.FromResult(currentFolder);
    public void RevealFile(string path) => RevealedPath = path;
}
