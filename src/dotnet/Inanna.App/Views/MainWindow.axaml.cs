using Avalonia.Controls;
using Avalonia.Input.Platform;
using Avalonia.Interactivity;
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

    private async void OnPasteUrlClick(object? sender, RoutedEventArgs eventArgs)
    {
        if (DataContext is not MainWindowViewModel viewModel ||
            !viewModel.CanEdit || viewModel.HasUrl || Clipboard is not { } clipboard)
        {
            return;
        }

        try
        {
            var text = await clipboard.TryGetTextAsync();
            // A clipboard read can finish after the user starts typing or downloading.
            if (!string.IsNullOrEmpty(text) && viewModel.CanEdit && !viewModel.HasUrl)
            {
                viewModel.Url = text;
                UrlInput.Focus();
                UrlInput.CaretIndex = text.Length;
            }
        }
        catch (Exception)
        {
            viewModel.StatusText = "Could not read the clipboard. Try pasting the URL manually.";
        }
    }
}
