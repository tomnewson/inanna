$ErrorActionPreference = "Stop"

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$project = Join-Path $repositoryRoot "src/dotnet/Inanna.App/Inanna.App.csproj"
$backend = Join-Path $repositoryRoot "target/x86_64-pc-windows-msvc/debug/inanna-backend.exe"
$appOutput = Join-Path $repositoryRoot "src/dotnet/Inanna.App/bin/Debug/net10.0"
$previousBackendVersion = $env:INANNA_VERSION

Push-Location $repositoryRoot
try {
    $versionTag = (& git describe --tags --abbrev=0 --match "v[0-9]*").Trim()
    if ($LASTEXITCODE -ne 0 -or $versionTag -notmatch '^v(\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?)$') {
        throw "Could not determine a semantic build version from the repository's Git tags."
    }
    $buildVersion = $versionTag.Substring(1)
    $env:INANNA_VERSION = $buildVersion

    & cargo build --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) {
        throw "The Rust backend build failed with exit code $LASTEXITCODE."
    }

    & dotnet build $project -p:Version=$buildVersion
    if ($LASTEXITCODE -ne 0) {
        throw "The .NET frontend build failed with exit code $LASTEXITCODE."
    }

    Copy-Item -LiteralPath $backend -Destination $appOutput -Force

    & dotnet run --project $project --no-build
    if ($LASTEXITCODE -ne 0) {
        throw "The application exited with code $LASTEXITCODE."
    }
}
finally {
    $env:INANNA_VERSION = $previousBackendVersion
    Pop-Location
}
