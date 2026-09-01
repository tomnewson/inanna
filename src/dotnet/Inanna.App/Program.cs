using Avalonia;
using Inanna.Services;
using Velopack;

namespace Inanna;

internal static class Program
{
    [STAThread]
    public static void Main(string[] args)
    {
        VelopackApp.Build().Run();

        // Velopack replaces the existing macOS bundle at its current path, so an
        // upgraded installation keeps its old Finder name. Rename it only after
        // Velopack has handled this launch, then restart before loading bundle
        // resources from the now-stale AppContext.BaseDirectory path.
        var renamedExecutable = MacApplicationBundleMigration.TryRenameCurrentBundle();
        if (renamedExecutable is not null)
        {
            MacApplicationBundleMigration.TryStartRenamedApplication(renamedExecutable);
            return;
        }

        BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);
    }

    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<App>()
            .UsePlatformDetect()
            .LogToTrace();
}
