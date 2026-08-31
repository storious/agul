param(
    [string]$Version = $(if ($env:AGUL_VERSION) { $env:AGUL_VERSION } else { "{{VERSION}}" }),
    [string]$InstallDir = $(
        if ($env:AGUL_INSTALL_DIR) { $env:AGUL_INSTALL_DIR }
        else { Join-Path $env:LOCALAPPDATA "Programs\Agul\bin" }
    ),
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$Version = $Version.Trim()
if ($Version -match '^\{\{.+\}\}$') {
    throw "this checkout installer has no embedded release; pass -Version or set AGUL_VERSION"
}
$Version = $Version.TrimStart("v")
if (-not [Environment]::Is64BitOperatingSystem -or
    [Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne
        [Runtime.InteropServices.Architecture]::X64) {
    throw "Agul currently publishes a Windows x64 binary"
}

$Target = "x86_64-pc-windows-msvc"
$Archive = "agul-v$Version-$Target.zip"
$Url = "https://github.com/storious/agul/releases/download/v$Version/$Archive"
$Destination = Join-Path $InstallDir "agul.exe"
$GitHubCli = Get-Command gh -ErrorAction SilentlyContinue
$UseGitHubCli = $false
if ($GitHubCli) {
    & gh auth status *> $null
    $UseGitHubCli = $LASTEXITCODE -eq 0
}

Write-Output "Agul $Version -> $Destination"
if ($DryRun) {
    if ($UseGitHubCli) {
        Write-Output "gh release download v$Version --repo storious/agul --pattern $Archive"
    } else {
        Write-Output $Url
    }
    return
}

$Temporary = Join-Path ([IO.Path]::GetTempPath()) ("agul-install-" + [Guid]::NewGuid())
try {
    New-Item -ItemType Directory -Path $Temporary | Out-Null
    $ArchivePath = Join-Path $Temporary $Archive
    if ($UseGitHubCli) {
        & gh release download "v$Version" `
            --repo storious/agul `
            --pattern $Archive `
            --dir $Temporary
        if ($LASTEXITCODE -ne 0) {
            throw "gh release download failed with exit code $LASTEXITCODE"
        }
    } else {
        Invoke-WebRequest -Uri $Url -OutFile $ArchivePath
    }
    Expand-Archive -LiteralPath $ArchivePath -DestinationPath $Temporary
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $Source = Join-Path $Temporary "agul-v$Version-$Target\agul.exe"
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathEntries = @($UserPath -split ";" | Where-Object { $_ })
    if (-not ($PathEntries | Where-Object { $_.TrimEnd("\") -ieq $InstallDir.TrimEnd("\") })) {
        $UpdatedPath = (@($PathEntries) + $InstallDir) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $UpdatedPath, "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Output "Added $InstallDir to the user PATH"
    }
    Write-Output "Installed $Destination"
} finally {
    if (Test-Path -LiteralPath $Temporary) {
        Remove-Item -LiteralPath $Temporary -Recurse -Force
    }
}
