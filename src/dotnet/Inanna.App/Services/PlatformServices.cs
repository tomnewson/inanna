using System.Diagnostics;
using System.Text;
using Avalonia.Controls;
using Avalonia.Platform.Storage;

namespace Inanna.Services;

public abstract class PlatformServices(Func<Window?> getWindow, ApplicationPaths paths) : IPlatformServices
{
    private static readonly Encoding Utf8 = new UTF8Encoding(false);
    private readonly Func<Window?> _getWindow = getWindow;

    protected string DataRoot => paths.DataRoot;
    protected abstract string BackendFileName { get; }

    public static IPlatformServices Create(
        Func<Window?> getWindow,
        ApplicationPaths? paths = null)
    {
        paths ??= ApplicationPaths.Create();
        if (OperatingSystem.IsWindows())
        {
            return new WindowsPlatformServices(getWindow, paths);
        }

        if (OperatingSystem.IsMacOS())
        {
            return new MacOsPlatformServices(getWindow, paths);
        }

        return new UnsupportedPlatformServices(getWindow, paths);
    }

    public virtual ProcessStartInfo CreateBackendStartInfo()
    {
        var info = new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, BackendFileName))
        {
            UseShellExecute = false,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            StandardInputEncoding = Utf8,
            StandardOutputEncoding = Utf8,
            StandardErrorEncoding = Utf8,
            CreateNoWindow = true,
        };
        info.ArgumentList.Add("--data-root");
        info.ArgumentList.Add(DataRoot);
        return info;
    }

    public async Task<string?> PickOutputFolderAsync(string? currentFolder)
    {
        var storage = _getWindow()?.StorageProvider;
        if (storage is null)
        {
            return null;
        }

        var start = await ResolveStartFolderAsync(storage, currentFolder);

        var selected = await storage.OpenFolderPickerAsync(new FolderPickerOpenOptions
        {
            Title = "Choose output folder",
            AllowMultiple = false,
            SuggestedStartLocation = start,
        });
        return selected.Count == 0 ? null : selected[0].TryGetLocalPath();
    }

    public abstract void RevealFile(string path);

    private static async Task<IStorageFolder?> ResolveStartFolderAsync(
        IStorageProvider storage,
        string? currentFolder)
    {
        if (string.IsNullOrWhiteSpace(currentFolder))
        {
            return null;
        }

        try
        {
            return await storage.TryGetFolderFromPathAsync(new Uri(Path.GetFullPath(currentFolder)));
        }
        catch (Exception)
        {
            return null;
        }
    }

    protected static void StartDetached(string executable, params string[] arguments)
    {
        var info = new ProcessStartInfo(executable) { UseShellExecute = false };
        foreach (var argument in arguments)
        {
            info.ArgumentList.Add(argument);
        }

        Process.Start(info);
    }
}

internal sealed class WindowsPlatformServices : PlatformServices
{
    public WindowsPlatformServices(Func<Window?> getWindow, ApplicationPaths? paths = null)
        : base(getWindow, paths ?? ApplicationPaths.Create())
    {
    }

    protected override string BackendFileName => "inanna-backend.exe";

    public override void RevealFile(string path) => StartDetached("explorer.exe", "/select,", path);
}

internal sealed class MacOsPlatformServices : PlatformServices
{
    public MacOsPlatformServices(Func<Window?> getWindow, ApplicationPaths? paths = null)
        : base(getWindow, paths ?? ApplicationPaths.Create())
    {
    }

    protected override string BackendFileName => "inanna-backend";

    public override void RevealFile(string path) => StartDetached("/usr/bin/open", "-R", path);
}

internal sealed class UnsupportedPlatformServices : PlatformServices
{
    public UnsupportedPlatformServices(Func<Window?> getWindow, ApplicationPaths? paths = null)
        : base(getWindow, paths ?? ApplicationPaths.Create())
    {
    }

    protected override string BackendFileName => "inanna-backend";

    public override void RevealFile(string path) => throw new PlatformNotSupportedException();
}
