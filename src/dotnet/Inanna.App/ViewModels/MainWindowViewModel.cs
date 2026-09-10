using System.Text.Json;
using Avalonia.Threading;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using Inanna.Services;

namespace Inanna.ViewModels;

public partial class MainWindowViewModel : ObservableObject
{
    private readonly IBackendClient _backend;
    private readonly IPlatformServices _platform;
    private readonly IApplicationUpdater _applicationUpdater;
    private readonly Action<Action> _dispatch;
    private string? _activeOperationId;
    private bool _toolsReadyBeforeOperation;
    private ApplicationUpdate? _applicationUpdate;
    private bool _checkingApplicationUpdate;
    private string? _deferredApplicationVersion;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanDownload))]
    [NotifyPropertyChangedFor(nameof(HasUrl))]
    private string _url = string.Empty;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanDownload))]
    private string _outputFolder = string.Empty;

    [ObservableProperty]
    private double _videoQuality = 3;

    [ObservableProperty]
    [NotifyPropertyChangedFor(
        nameof(CanEdit),
        nameof(CanBrowse),
        nameof(CanDownload),
        nameof(ShowDownloadButton),
        nameof(ShowUpdatePanel),
        nameof(ShowCancelButton),
        nameof(IsProgressIndeterminate),
        nameof(ShowRestartButton),
        nameof(CanInstallApplicationUpdate))]
    private bool _busy = true;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ShowCancelButton))]
    private bool _cancellable;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanEdit), nameof(CanDownload))]
    private bool _toolsReady;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ShowUpdatePanel))]
    private bool _setupRequired;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ShowUpdatePanel))]
    private bool _updateAvailable;

    [ObservableProperty]
    private bool _canInstallTools;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasCompletedFile), nameof(ShowStatusText))]
    private bool _completed;

    [ObservableProperty]
    private bool _failed;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(DetailsButtonText))]
    private bool _showDetails;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsProgressIndeterminate))]
    private double _progress;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ShowStatusText))]
    private string _statusText = "Starting the download engine…";

    [ObservableProperty]
    private string _updateSummary = string.Empty;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ShowDetailsButton))]
    private string _detailsText = string.Empty;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasCompletedFile), nameof(ShowStatusText))]
    private string? _completedPath;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(ShowRestartButton))]
    private bool _engineUnavailable;

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanInstallApplicationUpdate))]
    private bool _applicationUpdateAvailable;

    [ObservableProperty]
    [NotifyPropertyChangedFor(
        nameof(CanInstallApplicationUpdate),
        nameof(ShowUpdatePanel),
        nameof(CanEdit),
        nameof(CanBrowse),
        nameof(CanDownload))]
    private bool _applicationUpdateBusy;

    [ObservableProperty]
    private double _applicationUpdateProgress;

    [ObservableProperty]
    private string _applicationUpdateStatus = string.Empty;

    public MainWindowViewModel(
        IBackendClient backend,
        IPlatformServices platform,
        IApplicationUpdater applicationUpdater)
        : this(backend, platform, action => Dispatcher.UIThread.Post(action), applicationUpdater)
    {
    }

    internal MainWindowViewModel(
        IBackendClient backend,
        IPlatformServices platform,
        Action<Action> dispatch,
        IApplicationUpdater? applicationUpdater = null)
    {
        _backend = backend;
        _platform = platform;
        _applicationUpdater = applicationUpdater ?? UnavailableApplicationUpdater.Instance;
        _dispatch = dispatch;
        _backend.EventReceived += OnBackendEvent;
        _backend.BackendExited += OnBackendExited;
    }

    public bool CanEdit => ToolsReady && !Busy && !ApplicationUpdateBusy;
    public bool CanBrowse => !Busy && !ApplicationUpdateBusy;
    public bool HasUrl => !string.IsNullOrEmpty(Url);
    public bool CanDownload =>
        CanEdit &&
        !string.IsNullOrWhiteSpace(Url) && !string.IsNullOrWhiteSpace(OutputFolder);
    public bool ShowDownloadButton => !Busy;
    public bool ShowCancelButton => Busy && Cancellable;
    public bool ShowUpdatePanel => UpdateAvailable && !SetupRequired && !Busy && !ApplicationUpdateBusy;
    public bool ShowDetailsButton => !string.IsNullOrWhiteSpace(DetailsText);
    public bool IsProgressIndeterminate => Busy && Progress <= 0;
    public bool HasCompletedFile => Completed && !string.IsNullOrWhiteSpace(CompletedPath);
    public bool ShowStatusText =>
        !HasCompletedFile && !string.IsNullOrWhiteSpace(StatusText) && StatusText != "Ready.";
    public string DetailsButtonText => ShowDetails ? "Hide details" : "Details";
    public bool ShowRestartButton => EngineUnavailable && !Busy;
    public bool CanInstallApplicationUpdate =>
        ApplicationUpdateAvailable && !ApplicationUpdateBusy && !Busy;
    public async Task InitializeAsync()
    {
        var applicationUpdateCheck = CheckForApplicationUpdateAsync();
        try
        {
            Busy = true;
            await _backend.StartAsync();
            using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(5));
            var initialized = await _backend.SendAsync("initialize", cancellationToken: timeout.Token);
            var backendVersion = initialized.GetProperty("backendVersion").GetString();
            var frontendVersion = ApplicationVersion.Current;
            if (backendVersion != frontendVersion)
            {
                throw new InvalidDataException(
                    $"Frontend version {frontendVersion ?? "unknown"} cannot use backend version {backendVersion ?? "unknown"}.");
            }
            EngineUnavailable = false;
            OutputFolder = initialized.GetProperty("outputFolder").GetString() ?? string.Empty;
            ToolsReady = initialized.GetProperty("toolsReady").GetBoolean();
            Busy = !ToolsReady;
            StatusText = ToolsReady ? "Ready." : "Checking required tools…";
            await CheckToolsAsync();
        }
        catch (Exception error)
        {
            ApplyFailure("The download engine could not be started.", error);
            EngineUnavailable = true;
        }
        await applicationUpdateCheck;
    }

    internal async Task CheckForApplicationUpdateAsync()
    {
        if (!_applicationUpdater.CanUpdate || ApplicationUpdateBusy || _checkingApplicationUpdate)
        {
            return;
        }

        _checkingApplicationUpdate = true;
        try
        {
            var update = await _applicationUpdater.CheckForUpdatesAsync();
            if (ApplicationUpdateBusy)
            {
                return;
            }
            // A throttled check can return null. Keep an already offered update usable.
            if (update is not null)
            {
                _applicationUpdate = update;
                ApplicationUpdateAvailable = update.Version != _deferredApplicationVersion;
                ApplicationUpdateStatus = ApplicationUpdateAvailable
                    ? $"Version {update.Version} is available."
                    : "Update deferred until the next launch.";
            }
            else if (_applicationUpdate is null)
            {
                ApplicationUpdateStatus = "The application is up to date.";
            }
        }
        catch (Exception error)
        {
            if (_applicationUpdate is null)
            {
                ApplicationUpdateStatus = $"Could not check for updates: {error.Message}";
            }
        }
        finally
        {
            _checkingApplicationUpdate = false;
        }
    }

    [RelayCommand]
    private async Task InstallApplicationUpdateAsync()
    {
        if (_applicationUpdate is null || !CanInstallApplicationUpdate)
        {
            return;
        }

        ApplicationUpdateBusy = true;
        ApplicationUpdateProgress = 0;
        ApplicationUpdateStatus = $"Downloading version {_applicationUpdate.Version}...";
        try
        {
            await _applicationUpdater.DownloadAsync(
                _applicationUpdate,
                value => _dispatch(() => ApplicationUpdateProgress = value));
            ApplicationUpdateStatus = "Restarting to apply the update...";
            await _backend.StopAsync();
            _applicationUpdater.ApplyAndRestart(_applicationUpdate);
        }
        catch (Exception error)
        {
            ApplicationUpdateStatus = $"The update failed: {error.Message}";
            ApplicationUpdateBusy = false;
        }
    }

    [RelayCommand]
    private void DeferApplicationUpdate()
    {
        _deferredApplicationVersion = _applicationUpdate?.Version;
        ApplicationUpdateAvailable = false;
        ApplicationUpdateStatus = "Update deferred until the next launch.";
    }

    [RelayCommand]
    private async Task RestartBackendAsync()
    {
        EngineUnavailable = false;
        Busy = true;
        StatusText = "Restarting the download engine…";
        await IgnoreBackendStopFailureAsync();
        await InitializeAsync();
    }

    [RelayCommand]
    private async Task BrowseOutputAsync()
    {
        try
        {
            var selected = await _platform.PickOutputFolderAsync(OutputFolder);
            if (selected is null)
            {
                return;
            }

            await _backend.SendAsync("setOutputFolder", new { path = selected });
            OutputFolder = selected;
        }
        catch (Exception error)
        {
            ApplyFailure("The output folder could not be saved.", error);
        }
    }

    [RelayCommand]
    private async Task CheckToolsAsync()
    {
        var setup = !ToolsReady;
        if (setup)
        {
            SetWorking("Checking required tools…", false);
        }
        SetupRequired = false;
        UpdateAvailable = false;
        CanInstallTools = false;
        try
        {
            var result = await _backend.SendAsync("checkTools");
            if (EngineUnavailable)
            {
                return;
            }
            if (setup)
            {
                Busy = false;
            }
            ToolsReady = result.GetProperty("toolsReady").GetBoolean();
            CanInstallTools = result.GetProperty("canInstallTools").GetBoolean();
            UpdateSummary = result.GetProperty("updateSummary").GetString() ?? string.Empty;
            if (setup || StatusText == "Ready.")
            {
                StatusText = result.GetProperty("statusText").GetString() ?? string.Empty;
            }
            var state = result.GetProperty("state").GetString();
            SetupRequired = state == "setupRequired";
            UpdateAvailable = state == "updateAvailable";
        }
        catch (Exception error)
        {
            if (!setup || EngineUnavailable)
            {
                return;
            }
            ToolsReady = false;
            SetupRequired = true;
            CanInstallTools = false;
            ApplyFailure("Could not check required tools.", error);
            UpdateSummary = "An internet connection is required for the first setup.";
        }
    }

    [RelayCommand]
    private async Task InstallToolsAsync()
    {
        if (Busy || ApplicationUpdateBusy)
        {
            return;
        }
        _toolsReadyBeforeOperation = ToolsReady;
        SetWorking("Preparing tool installation…", true);
        ToolsReady = false;
        CanInstallTools = false;
        try
        {
            var result = await _backend.SendAsync("installTools");
            _activeOperationId = result.GetProperty("operationId").GetString();
        }
        catch (Exception error)
        {
            ToolsReady = _toolsReadyBeforeOperation;
            ApplyFailure("Tool installation failed.", error);
            SetupRequired = !ToolsReady;
            CanInstallTools = true;
        }
    }

    [RelayCommand]
    private void DeferUpdate()
    {
        UpdateAvailable = false;
        CanInstallTools = false;
        if (!Busy)
        {
            StatusText = "Ready. The cached tools will be used.";
        }
    }

    [RelayCommand]
    private async Task StartDownloadAsync()
    {
        SetWorking("Preparing download…", true);
        Completed = false;
        CompletedPath = null;
        Progress = 0;
        try
        {
            var selectedQuality = (int)Math.Round(VideoQuality);
            var quality = selectedQuality switch
            {
                1 => "p1080",
                2 => "p1440",
                _ => "best",
            };
            var result = await _backend.SendAsync("startDownload", new
            {
                url = Url,
                mode = selectedQuality <= 0 ? "audioOnly" : "video",
                videoQuality = quality,
                outputFolder = OutputFolder,
            });
            _activeOperationId = result.GetProperty("operationId").GetString();
        }
        catch (Exception error)
        {
            ApplyFailure("The download failed.", error);
        }
    }

    [RelayCommand]
    private async Task CancelAsync()
    {
        if (_activeOperationId is null)
        {
            return;
        }

        Cancellable = false;
        StatusText = "Cancelling…";
        try
        {
            await _backend.SendAsync("cancel", new { operationId = _activeOperationId });
        }
        catch (Exception error)
        {
            ApplyFailure("The operation could not be cancelled.", error);
        }
    }

    [RelayCommand]
    private void OpenFolder()
    {
        if (CompletedPath is not null)
        {
            _platform.RevealFile(CompletedPath);
        }
    }

    [RelayCommand]
    private void ToggleDetails() => ShowDetails = !ShowDetails;

    [RelayCommand]
    private void ClearSearch() => Url = string.Empty;

    private void OnBackendEvent(BackendEvent message) =>
        _dispatch(() => HandleBackendEvent(message));

    private void HandleBackendEvent(BackendEvent message)
    {
        if (message.OperationId != _activeOperationId)
        {
            return;
        }

        var operationKind = message.Data.TryGetProperty("operationKind", out var kind)
            ? kind.GetString()
            : null;
        switch (message.Event)
        {
            case "operationProgress":
                StatusText = message.Data.GetProperty("message").GetString() ?? StatusText;
                if (message.Data.TryGetProperty("fraction", out var fraction) &&
                    fraction.ValueKind == JsonValueKind.Number)
                {
                    Progress = fraction.GetDouble() * 100;
                }
                break;
            case "operationCompleted" when operationKind == "toolInstall":
                FinishOperation();
                ToolsReady = true;
                _toolsReadyBeforeOperation = false;
                SetupRequired = false;
                UpdateAvailable = false;
                CanInstallTools = false;
                StatusText = string.Empty;
                break;
            case "operationCompleted" when operationKind == "download":
                FinishOperation();
                Completed = true;
                Progress = 100;
                CompletedPath = message.Data.GetProperty("path").GetString();
                StatusText = CompletedPath is null ? "Download complete." : $"Saved {CompletedPath}";
                break;
            case "operationCancelled":
                FinishOperation();
                Progress = 0;
                ToolsReady = operationKind == "toolInstall" ? _toolsReadyBeforeOperation : true;
                CanInstallTools = operationKind == "toolInstall";
                SetupRequired = operationKind == "toolInstall" && !ToolsReady;
                StatusText = operationKind == "toolInstall"
                    ? "Tool installation cancelled."
                    : "Download cancelled.";
                break;
            case "operationFailed":
                FinishOperation();
                var error = message.Data.GetProperty("error");
                StatusText = error.GetProperty("message").GetString() ?? "The operation failed.";
                DetailsText = error.TryGetProperty("details", out var details) && details.ValueKind == JsonValueKind.String
                    ? details.GetString() ?? string.Empty
                    : string.Empty;
                Failed = true;
                CanInstallTools = operationKind == "toolInstall";
                ToolsReady = operationKind == "toolInstall" ? _toolsReadyBeforeOperation : ToolsReady;
                SetupRequired = operationKind == "toolInstall" && !ToolsReady;
                break;
        }
    }

    private void OnBackendExited(string? message) => _dispatch(() =>
    {
        _activeOperationId = null;
        ApplyFailure("The download engine stopped unexpectedly.", message ?? "The backend exited.");
        ToolsReady = false;
        EngineUnavailable = true;
    });

    private void SetWorking(string status, bool cancellable)
    {
        Busy = true;
        Cancellable = cancellable;
        Failed = false;
        ShowDetails = false;
        DetailsText = string.Empty;
        StatusText = status;
    }

    private void FinishOperation()
    {
        _activeOperationId = null;
        Busy = false;
        Cancellable = false;
        Failed = false;
    }

    private void ApplyFailure(string status, object error)
    {
        Busy = false;
        Cancellable = false;
        Failed = true;
        StatusText = error is BackendRequestException requestError && requestError.Error.Code == "invalidUrl"
            ? requestError.Error.Message
            : status;
        DetailsText = error switch
        {
            BackendRequestException backend => backend.Error.Details ?? backend.Error.Message,
            Exception exception => exception.Message,
            _ => error.ToString() ?? string.Empty,
        };
    }

    private async Task IgnoreBackendStopFailureAsync()
    {
        try
        {
            await _backend.StopAsync();
        }
        catch (Exception)
        {
        }
    }
}
