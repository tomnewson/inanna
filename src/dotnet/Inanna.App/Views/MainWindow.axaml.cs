using Avalonia.Controls;
using Avalonia.Platform;
using Avalonia.Threading;
using Inanna.ViewModels;

namespace Inanna.Views;

public partial class MainWindow : Window
{
    private bool _initialized;

    public MainWindow()
    {
        InitializeComponent();
        if (OperatingSystem.IsWindows())
        {
            Icon = new WindowIcon(
                AssetLoader.Open(new Uri("avares://yt-dlp-wrapper/Assets/app-icon.ico")));
        }
        Opened += OnOpened;
    }

    private void OnOpened(object? sender, EventArgs eventArgs)
    {
        if (_initialized || DataContext is not MainWindowViewModel viewModel)
        {
            return;
        }

        _initialized = true;
        Dispatcher.UIThread.Post(
            () => _ = viewModel.InitializeAsync(),
            DispatcherPriority.Loaded);
    }
}
