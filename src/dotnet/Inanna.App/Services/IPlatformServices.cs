using System.Diagnostics;

namespace Inanna.Services;

public interface IPlatformServices
{
    ProcessStartInfo CreateBackendStartInfo();
    Task<string?> PickOutputFolderAsync(string? currentFolder);
    void RevealFile(string path);
}
