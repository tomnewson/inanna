using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using Avalonia.Threading;
using Inanna.Services;
using Inanna.ViewModels;
using Inanna.Views;

namespace Inanna;

public partial class App : Application
{
    private BackendClient? _backend;
    private bool _shutdownStarted;

    public override void Initialize() => AvaloniaXamlLoader.Load(this);

    public override void OnFrameworkInitializationCompleted()
    {
        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            MainWindow? window = null;
            var paths = ApplicationPaths.Create();
            var platform = PlatformServices.Create(() => window, paths);
            var updater = ApplicationUpdater.Create(paths);
            _backend = new BackendClient(platform);
            var viewModel = new MainWindowViewModel(_backend, platform, updater);
            window = new MainWindow { DataContext = viewModel };
            desktop.MainWindow = window;
            window.Closing += (_, eventArgs) =>
            {
                if (_shutdownStarted)
                {
                    return;
                }

                _shutdownStarted = true;
                eventArgs.Cancel = true;
                window.Hide();
                _ = FinishShutdownAsync(desktop);
            };
        }

        base.OnFrameworkInitializationCompleted();
    }

    private async Task FinishShutdownAsync(IClassicDesktopStyleApplicationLifetime desktop)
    {
        await IgnoreBackendShutdownFailureAsync();
        await Dispatcher.UIThread.InvokeAsync(() => desktop.Shutdown());
    }

    private async Task IgnoreBackendShutdownFailureAsync()
    {
        try
        {
            if (_backend is not null)
            {
                await Task.Run(() => _backend.DisposeAsync().AsTask());
            }
        }
        catch (Exception)
        {
        }
    }
}
