using System.Diagnostics;
using System.Text;
using Avalonia.Controls;
using Avalonia.Platform.Storage;

namespace Inanna.Services;

public sealed class PlatformServices(Func<Window?> getWindow, ApplicationPaths paths) : IPlatformServices
{
    private static readonly Encoding Utf8 = new UTF8Encoding(false);

    public static IPlatformServices Create(
        Func<Window?> getWindow,
        ApplicationPaths? paths = null) => new PlatformServices(getWindow, paths ?? ApplicationPaths.Create());

    public ProcessStartInfo CreateBackendStartInfo()
    {
        var backendFileName = OperatingSystem.IsWindows() ? "inanna-backend.exe" : "inanna-backend";
        var info = new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, backendFileName))
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
        info.ArgumentList.Add(paths.DataRoot);
        return info;
    }

    public async Task<string?> PickOutputFolderAsync(string? currentFolder)
    {
        var storage = getWindow()?.StorageProvider;
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

    public void RevealFile(string path)
    {
        if (OperatingSystem.IsWindows())
        {
            StartDetached("explorer.exe", "/select,", path);
        }
        else if (OperatingSystem.IsMacOS())
        {
            StartDetached("/usr/bin/open", "-R", path);
        }
        else
        {
            throw new PlatformNotSupportedException();
        }
    }

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

    private static void StartDetached(string executable, params string[] arguments)
    {
        var info = new ProcessStartInfo(executable) { UseShellExecute = false };
        foreach (var argument in arguments)
        {
            info.ArgumentList.Add(argument);
        }

        Process.Start(info);
    }
}
