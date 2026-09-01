param(
    [Parameter(Mandatory = $true)]
    [string]$BackendPath
)

$dataRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("inanna-smoke-" + [guid]::NewGuid())
$info = [System.Diagnostics.ProcessStartInfo]::new((Resolve-Path $BackendPath).Path)
$info.UseShellExecute = $false
$info.RedirectStandardInput = $true
$info.RedirectStandardOutput = $true
$info.RedirectStandardError = $true
$info.CreateNoWindow = $true
$info.ArgumentList.Add("--data-root")
$info.ArgumentList.Add($dataRoot)
$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $info
$started = $false

function Read-ProtocolLine([System.IO.StreamReader]$reader) {
    $read = $reader.ReadLineAsync()
    if (-not $read.Wait(5000)) {
        throw "Timed out waiting for the backend protocol."
    }
    return $read.Result
}

try {
    if (-not $process.Start()) {
        throw "Backend failed to start."
    }
    $started = $true
    $request = '{"protocolVersion":1,"kind":"request","requestId":"smoke-init","method":"initialize","params":{}}'
    $process.StandardInput.WriteLine($request)
    $response = Read-ProtocolLine($process.StandardOutput) | ConvertFrom-Json
    if (-not $response.ok -or $response.requestId -ne "smoke-init") {
        throw "Backend handshake failed: $($response | ConvertTo-Json -Compress)"
    }

    $shutdown = '{"protocolVersion":1,"kind":"request","requestId":"smoke-stop","method":"shutdown","params":{}}'
    $process.StandardInput.WriteLine($shutdown)
    $response = Read-ProtocolLine($process.StandardOutput) | ConvertFrom-Json
    if (-not $response.ok -or $response.requestId -ne "smoke-stop") {
        throw "Backend shutdown failed."
    }
    if (-not $process.WaitForExit(5000)) {
        throw "Backend did not exit after shutdown."
    }
    if ($process.ExitCode -ne 0) {
        throw "Backend exited with code $($process.ExitCode)."
    }
}
finally {
    if ($started -and -not $process.HasExited) {
        $process.Kill($true)
    }
    $process.Dispose()
    if (Test-Path $dataRoot) {
        Remove-Item -Recurse -Force $dataRoot
    }
}
