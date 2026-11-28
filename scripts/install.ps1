param(
    [ValidatePattern('^(latest|v[0-9A-Za-z][0-9A-Za-z._+-]*)$')]
    [string]$Version = 'latest',

    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Star\bin'),

    [switch]$AddToPath
)

$ErrorActionPreference = 'Stop'
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)
$repository = 'soyuo/star.lang'
$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
switch ($architecture) {
    'X64' { $target = 'x86_64-pc-windows-msvc' }
    'Arm64' { $target = 'aarch64-pc-windows-msvc' }
    default { throw "Unsupported Windows architecture: $architecture" }
}

if ($Version -eq 'latest') {
    $releaseUrl = "https://github.com/$repository/releases/latest/download"
} else {
    $releaseUrl = "https://github.com/$repository/releases/download/$Version"
}
$archiveName = "star-$target.zip"
$temporaryDir = Join-Path ([System.IO.Path]::GetTempPath()) ("star-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temporaryDir | Out-Null

try {
    $archivePath = Join-Path $temporaryDir $archiveName
    $checksumsPath = Join-Path $temporaryDir 'SHA256SUMS'
    Invoke-WebRequest -Uri "$releaseUrl/$archiveName" -OutFile $archivePath
    Invoke-WebRequest -Uri "$releaseUrl/SHA256SUMS" -OutFile $checksumsPath

    $escapedName = [regex]::Escape($archiveName)
    $checksumMatch = [regex]::Match((Get-Content -Raw -LiteralPath $checksumsPath), "(?m)^([0-9a-fA-F]{64})\s+\*?$escapedName\r?$")
    if (-not $checksumMatch.Success) {
        throw "No checksum found for $archiveName"
    }
    $expectedHash = $checksumMatch.Groups[1].Value.ToLowerInvariant()
    $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash) {
        throw "Checksum verification failed for $archiveName"
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        $allowedEntries = @('star.exe', 'LICENSE', 'README.md')
        if ($zip.Entries.Count -ne $allowedEntries.Count) {
            throw 'The release archive does not contain the expected three files.'
        }
        $entryNames = @($zip.Entries | ForEach-Object { $_.FullName })
        if (@($entryNames | Select-Object -Unique).Count -ne $allowedEntries.Count) {
            throw 'The release archive contains duplicate entries.'
        }
        foreach ($entry in $zip.Entries) {
            if ($allowedEntries -notcontains $entry.FullName) {
                throw "Unexpected archive entry: $($entry.FullName)"
            }
            $unixFileType = ($entry.ExternalAttributes -shr 16) -band 0xF000
            if ($unixFileType -ne 0x8000 -or ($entry.ExternalAttributes -band 0x10) -ne 0) {
                throw "Archive member is not a regular file: $($entry.FullName)"
            }
        }
    } finally {
        $zip.Dispose()
    }

    $extractDir = Join-Path $temporaryDir 'extracted'
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $extractDir 'star.exe') -Destination (Join-Path $InstallDir 'star.exe') -Force

    if ($AddToPath) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $pathEntries = @($userPath -split ';' | Where-Object { $_ })
        if ($pathEntries -notcontains $InstallDir) {
            [Environment]::SetEnvironmentVariable('Path', (($pathEntries + $InstallDir) -join ';'), 'User')
        }
        Write-Output 'Added the install directory to the user PATH. Open a new terminal to use it.'
    } else {
        Write-Output "Add $InstallDir to PATH to use Star from any directory."
    }
    Write-Output "Installed Star to $(Join-Path $InstallDir 'star.exe')"
} finally {
    Remove-Item -LiteralPath $temporaryDir -Recurse -Force
}
