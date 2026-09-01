using System.Text.Json;

namespace Inanna.Services;

public interface IBackendClient : IAsyncDisposable
{
    event Action<BackendEvent>? EventReceived;
    event Action<string?>? BackendExited;

    Task StartAsync(CancellationToken cancellationToken = default);
    Task<JsonElement> SendAsync(
        string method,
        object? parameters = null,
        CancellationToken cancellationToken = default);
    Task StopAsync();
}

public sealed record BackendEvent(string OperationId, string Event, JsonElement Data);

public sealed record BackendWireError(string Code, string Message, string? Details);

public sealed class BackendRequestException(BackendWireError error) : Exception(error.Message)
{
    public BackendWireError Error { get; } = error;
}
