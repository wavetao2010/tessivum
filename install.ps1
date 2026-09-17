[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [AllowEmptyString()]
    [string]$Version,

    [switch]$Uninstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$script:LockStream = $null
$script:DefaultVersion = '0.1.0-alpha.27'
$script:ReleaseRepository = 'https://github.com/wavetao2010/tessivum'
$script:Target = 'x86_64-pc-windows-msvc'
$script:PathOwnerFileName = '.tessivum-path-owner'
$script:InstallMarkerFileName = '.tessivum-installed'

function Fail {
    param([Parameter(Mandatory = $true)][string]$Message)

    throw $Message
}

function Show-Usage {
    [Console]::Error.WriteLine('usage: powershell -ExecutionPolicy Bypass -File .\install.ps1 [version]')
    [Console]::Error.WriteLine('       powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall')
    exit 2
}

function Test-VersionName {
    param([Parameter(Mandatory = $true)][string]$Value)

    return $Value -match '^[0-9A-Za-z][0-9A-Za-z.-]*$' -and $Value.IndexOf('..', [System.StringComparison]::Ordinal) -lt 0
}

function Get-AbsolutePath {
    param(
        [Parameter(Mandatory = $true)][string]$Value,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ([string]::IsNullOrWhiteSpace($Value)) {
        Fail "$Label is required"
    }

    try {
        return [System.IO.Path]::GetFullPath($Value)
    }
    catch {
        Fail "invalid ${Label}: $Value"
    }
}

function Get-NormalizedPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    $fullPath = [System.IO.Path]::GetFullPath($Path)
    $root = [System.IO.Path]::GetPathRoot($fullPath)
    if ($null -eq $root) {
        Fail "path does not have a root: $Path"
    }

    if ($fullPath.Length -gt $root.Length) {
        $fullPath = $fullPath.TrimEnd([char[]]@('\', '/'))
    }

    return $fullPath
}

function Test-PathEquals {
    param(
        [Parameter(Mandatory = $true)][string]$Left,
        [Parameter(Mandatory = $true)][string]$Right
    )

    return [string]::Equals(
        (Get-NormalizedPath -Path $Left),
        (Get-NormalizedPath -Path $Right),
        [System.StringComparison]::OrdinalIgnoreCase
    )
}

function Test-PathWithinRoot {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Candidate,
        [switch]$AllowRoot
    )

    $normalizedRoot = Get-NormalizedPath -Path $Root
    $normalizedCandidate = Get-NormalizedPath -Path $Candidate
    if ([string]::Equals($normalizedRoot, $normalizedCandidate, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $AllowRoot.IsPresent
    }

    $separator = [System.IO.Path]::DirectorySeparatorChar
    return $normalizedCandidate.StartsWith(
        ($normalizedRoot + $separator),
        [System.StringComparison]::OrdinalIgnoreCase
    )
}

function Assert-NotFilesystemRoot {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $normalized = Get-NormalizedPath -Path $Path
    $root = [System.IO.Path]::GetPathRoot($normalized)
    if ($null -eq $root) {
        Fail "$Label does not have a filesystem root"
    }

    if ([string]::Equals(
        $normalized.TrimEnd([char[]]@('\', '/')),
        $root.TrimEnd([char[]]@('\', '/')),
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        Fail "$Label must not be a filesystem root"
    }
}

function Get-ExistingItem {
    param([Parameter(Mandatory = $true)][string]$Path)

    try {
        return Get-Item -LiteralPath $Path -Force -ErrorAction Stop
    }
    catch [System.Management.Automation.ItemNotFoundException] {
        return $null
    }
    catch [System.IO.FileNotFoundException] {
        return $null
    }
    catch [System.IO.DirectoryNotFoundException] {
        return $null
    }
}

function Test-ReparsePoint {
    param([Parameter(Mandatory = $true)]$Item)

    return (([int]$Item.Attributes -band [int][System.IO.FileAttributes]::ReparsePoint) -ne 0)
}

function Ensure-NormalDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )

    [System.IO.Directory]::CreateDirectory($Path) | Out-Null
    $item = Get-ExistingItem -Path $Path
    if ($null -eq $item -or -not $item.PSIsContainer) {
        Fail "$Label is not a directory: $Path"
    }
    if (Test-ReparsePoint -Item $item) {
        Fail "$Label must not be a reparse point: $Path"
    }

    return $item.FullName
}

function Assert-NormalDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $item = Get-ExistingItem -Path $Path
    if ($null -eq $item -or -not $item.PSIsContainer) {
        Fail "$Label is not a directory: $Path"
    }
    if (Test-ReparsePoint -Item $item) {
        Fail "$Label must not be a reparse point: $Path"
    }

    return $item.FullName
}

function Assert-RegularFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $item = Get-ExistingItem -Path $Path
    if ($null -eq $item -or $item.PSIsContainer) {
        Fail "$Label is missing or is not a file: $Path"
    }
    if (Test-ReparsePoint -Item $item) {
        Fail "$Label must not be a reparse point: $Path"
    }

    return $item.FullName
}

function Assert-TreeHasNoReparsePoints {
    param([Parameter(Mandatory = $true)][string]$Root)

    $rootDirectory = Assert-NormalDirectory -Path $Root -Label 'archive root'
    $stack = New-Object System.Collections.Stack
    $stack.Push([System.IO.DirectoryInfo]::new($rootDirectory))

    while ($stack.Count -gt 0) {
        $directory = [System.IO.DirectoryInfo]$stack.Pop()
        foreach ($child in $directory.EnumerateFileSystemInfos()) {
            if (([int]$child.Attributes -band [int][System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                Fail "reparse point is not allowed in an installed release: $($child.FullName)"
            }
            if ($child -is [System.IO.DirectoryInfo]) {
                $stack.Push($child)
            }
        }
    }
}

function Get-RelativePath {
    param(
        [Parameter(Mandatory = $true)][string]$FromDirectory,
        [Parameter(Mandatory = $true)][string]$ToPath
    )

    $from = Get-NormalizedPath -Path $FromDirectory
    $to = Get-NormalizedPath -Path $ToPath
    $fromRoot = [System.IO.Path]::GetPathRoot($from)
    $toRoot = [System.IO.Path]::GetPathRoot($to)
    if (-not [string]::Equals($fromRoot, $toRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        Fail 'BIN_DIR and INSTALL_ROOT must be on the same volume'
    }

    $fromTail = $from.Substring($fromRoot.Length).Trim([char[]]@('\', '/'))
    $toTail = $to.Substring($toRoot.Length).Trim([char[]]@('\', '/'))
    [string[]]$fromParts = @()
    if (-not [string]::IsNullOrEmpty($fromTail)) {
        $fromParts = $fromTail.Split([char[]]@('\'), [System.StringSplitOptions]::RemoveEmptyEntries)
    }
    [string[]]$toParts = @()
    if (-not [string]::IsNullOrEmpty($toTail)) {
        $toParts = $toTail.Split([char[]]@('\'), [System.StringSplitOptions]::RemoveEmptyEntries)
    }

    $common = 0
    while (
        $common -lt $fromParts.Count -and
        $common -lt $toParts.Count -and
        [string]::Equals($fromParts[$common], $toParts[$common], [System.StringComparison]::OrdinalIgnoreCase)
    ) {
        $common++
    }

    $parts = [System.Collections.Generic.List[string]]::new()
    for ($index = $common; $index -lt $fromParts.Count; $index++) {
        [void]$parts.Add('..')
    }
    for ($index = $common; $index -lt $toParts.Count; $index++) {
        [void]$parts.Add($toParts[$index])
    }

    if ($parts.Count -eq 0) {
        return '.'
    }

    return [string]::Join('\', $parts.ToArray())
}

function Escape-CmdBatchPath {
    param([Parameter(Mandatory = $true)][string]$Value)

    return $Value.Replace('%', '%%')
}

function New-ManagedLauncherContent {
    param([Parameter(Mandatory = $true)][string]$RelativeTarget)

    $escapedTarget = Escape-CmdBatchPath -Value $RelativeTarget
    $lines = @(
        '@echo off',
        'rem TESSIVUM-MANAGED-LAUNCHER: 1',
        ('rem TESSIVUM-TARGET: ' + $RelativeTarget),
        'setlocal DisableDelayedExpansion',
        ('call "%~dp0' + $escapedTarget + '" %*'),
        'exit /b %ERRORLEVEL%'
    )

    return ([string]::Join("`r`n", $lines) + "`r`n")
}

function Get-ManagedLauncherInfo {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$LauncherName,
        [Parameter(Mandatory = $true)][string]$BinDirectory,
        [Parameter(Mandatory = $true)][string]$InstallRoot
    )

    $item = Get-ExistingItem -Path $Path
    if ($null -eq $item -or $item.PSIsContainer -or (Test-ReparsePoint -Item $item)) {
        return $null
    }

    try {
        $content = [System.IO.File]::ReadAllText($Path, $script:Utf8NoBom)
    }
    catch {
        return $null
    }

    $pattern = '\A@echo off\r?\nrem TESSIVUM-MANAGED-LAUNCHER: 1\r?\nrem TESSIVUM-TARGET: (?<target>[^\r\n]+)\r?\nsetlocal DisableDelayedExpansion\r?\ncall "%~dp0(?<called>[^"\r\n]+)" %\*\r?\nexit /b %ERRORLEVEL%\r?\n\z'
    $match = [System.Text.RegularExpressions.Regex]::Match(
        $content,
        $pattern,
        [System.Text.RegularExpressions.RegexOptions]::CultureInvariant
    )
    if (-not $match.Success) {
        return $null
    }

    $relativeTarget = $match.Groups['target'].Value
    if (
        [System.IO.Path]::IsPathRooted($relativeTarget) -or
        $relativeTarget.IndexOf([char]0) -ge 0 -or
        $relativeTarget.Contains('/')
    ) {
        return $null
    }
    if (-not [string]::Equals(
        $match.Groups['called'].Value,
        (Escape-CmdBatchPath -Value $relativeTarget),
        [System.StringComparison]::Ordinal
    )) {
        return $null
    }

    try {
        $targetPath = [System.IO.Path]::GetFullPath((Join-Path -Path $BinDirectory -ChildPath $relativeTarget))
    }
    catch {
        return $null
    }
    if (-not (Test-PathWithinRoot -Root $InstallRoot -Candidate $targetPath)) {
        return $null
    }
    if (-not [string]::Equals(
        [System.IO.Path]::GetFileName($targetPath),
        ($LauncherName + '.cmd'),
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        return $null
    }

    $targetBinDirectory = Split-Path -Parent $targetPath
    if (-not [string]::Equals(
        [System.IO.Path]::GetFileName($targetBinDirectory),
        'bin',
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        return $null
    }
    $versionDirectory = Split-Path -Parent $targetBinDirectory
    $targetVersion = [System.IO.Path]::GetFileName($versionDirectory)
    if (-not (Test-VersionName -Value $targetVersion)) {
        return $null
    }
    if (-not (Test-PathEquals -Left (Split-Path -Parent $versionDirectory) -Right $InstallRoot)) {
        return $null
    }

    try {
        $canonicalRelativeTarget = Get-RelativePath -FromDirectory $BinDirectory -ToPath $targetPath
    }
    catch {
        return $null
    }
    if (-not [string]::Equals(
        $relativeTarget,
        $canonicalRelativeTarget,
        [System.StringComparison]::Ordinal
    )) {
        return $null
    }

    $expectedContent = New-ManagedLauncherContent -RelativeTarget $relativeTarget
    if (-not [string]::Equals($content, $expectedContent, [System.StringComparison]::Ordinal)) {
        return $null
    }

    return [PSCustomObject]@{
        Path = $Path
        Version = $targetVersion
        Target = $targetPath
        Content = $content
    }
}

function Assert-ReplaceableLauncher {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$LauncherName,
        [Parameter(Mandatory = $true)][string]$BinDirectory,
        [Parameter(Mandatory = $true)][string]$InstallRoot
    )

    $item = Get-ExistingItem -Path $Path
    if ($null -eq $item) {
        return $null
    }

    $managed = Get-ManagedLauncherInfo `
        -Path $Path `
        -LauncherName $LauncherName `
        -BinDirectory $BinDirectory `
        -InstallRoot $InstallRoot
    if ($null -eq $managed) {
        Fail "refusing to replace unmanaged launcher: $Path"
    }

    return $managed
}

function Ensure-ZipSupport {
    if ($null -eq ('System.IO.Compression.ZipFile' -as [type])) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
    }
}

function Assert-ZipSegment {
    param([Parameter(Mandatory = $true)][string]$Segment)

    if (
        [string]::IsNullOrEmpty($Segment) -or
        $Segment -eq '.' -or
        $Segment -eq '..' -or
        $Segment.IndexOf([char]0) -ge 0 -or
        $Segment -match '[<>:"|?*\x00-\x1f]' -or
        $Segment.EndsWith('.') -or
        $Segment.EndsWith(' ')
    ) {
        Fail "unsafe ZIP path segment: $Segment"
    }

    $deviceName = ($Segment.Split('.')[0]).ToUpperInvariant()
    if ($deviceName -match '^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$') {
        Fail "unsafe ZIP device path: $Segment"
    }
}

function Assert-SafeZipEntryAttributes {
    param($Entry)

    $attributes = [int]$Entry.ExternalAttributes
    $unixType = (($attributes -shr 16) -band 0xf000)
    if ($unixType -eq 0xa000) {
        Fail "ZIP symlink entries are not allowed: $($Entry.FullName)"
    }
    if ($unixType -ne 0 -and $unixType -ne 0x4000 -and $unixType -ne 0x8000) {
        Fail "unsupported ZIP entry type: $($Entry.FullName)"
    }
    if (($attributes -band 0x0400) -ne 0) {
        Fail "ZIP reparse-point entries are not allowed: $($Entry.FullName)"
    }
}

function Expand-VerifiedZipArchive {
    param(
        [Parameter(Mandatory = $true)][string]$ArchivePath,
        [Parameter(Mandatory = $true)][string]$DestinationDirectory,
        [Parameter(Mandatory = $true)][string]$ArchiveRoot
    )

    Ensure-ZipSupport
    $archive = $null
    try {
        $archive = [System.IO.Compression.ZipFile]::OpenRead($ArchivePath)
        if ($archive.Entries.Count -eq 0) {
            Fail 'ZIP archive is empty'
        }

        $originalPaths = [System.Collections.Generic.Dictionary[string,string]]::new([System.StringComparer]::OrdinalIgnoreCase)
        $pathKinds = [System.Collections.Generic.Dictionary[string,string]]::new([System.StringComparer]::OrdinalIgnoreCase)
        $declaredEntries = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
        $plans = [System.Collections.Generic.List[object]]::new()
        $sawRoot = $false

        foreach ($entry in $archive.Entries) {
            $entryName = $entry.FullName
            if ([string]::IsNullOrEmpty($entryName)) {
                Fail 'ZIP archive contains an empty entry name'
            }
            if (
                $entryName.IndexOf([char]0) -ge 0 -or
                $entryName.Contains('\') -or
                $entryName.StartsWith('/') -or
                $entryName.StartsWith('\\') -or
                $entryName -match '^[A-Za-z]:'
            ) {
                Fail "unsafe ZIP entry path: $entryName"
            }
            Assert-SafeZipEntryAttributes -Entry $entry

            $isDirectory = $entryName.EndsWith('/')
            $trimmedEntryName = if ($isDirectory) {
                $entryName.Substring(0, $entryName.Length - 1)
            }
            else {
                $entryName
            }
            if ([string]::IsNullOrEmpty($trimmedEntryName)) {
                Fail "unsafe ZIP entry path: $entryName"
            }

            $segments = @($trimmedEntryName.Split([char[]]@('/'), [System.StringSplitOptions]::None))
            foreach ($segment in $segments) {
                Assert-ZipSegment -Segment $segment
            }
            if (-not [string]::Equals($segments[0], $ArchiveRoot, [System.StringComparison]::Ordinal)) {
                Fail "ZIP archive root must be exactly $ArchiveRoot"
            }
            if ($segments.Count -eq 1 -and -not $isDirectory) {
                Fail "ZIP archive root must be a directory: $ArchiveRoot"
            }
            $sawRoot = $true

            for ($index = 0; $index -lt $segments.Count; $index++) {
                $prefix = [string]::Join('\', $segments[0..$index])
                $requiredKind = if ($index -lt ($segments.Count - 1) -or $isDirectory) {
                    'directory'
                }
                else {
                    'file'
                }
                $knownOriginal = $null
                if ($originalPaths.TryGetValue($prefix, [ref]$knownOriginal)) {
                    if (-not [string]::Equals($knownOriginal, $prefix, [System.StringComparison]::Ordinal)) {
                        Fail "ZIP archive contains a case-colliding path: $entryName"
                    }
                    if (-not [string]::Equals($pathKinds[$prefix], $requiredKind, [System.StringComparison]::Ordinal)) {
                        Fail "ZIP archive contains a file/directory collision: $entryName"
                    }
                }
                else {
                    $originalPaths.Add($prefix, $prefix)
                    $pathKinds.Add($prefix, $requiredKind)
                }

                if ($index -eq ($segments.Count - 1)) {
                    if (-not $declaredEntries.Add($prefix)) {
                        Fail "ZIP archive contains a duplicate path: $entryName"
                    }
                }
            }

            [void]$plans.Add([PSCustomObject]@{
                Entry = $entry
                Segments = $segments
                IsDirectory = $isDirectory
            })
        }

        if (-not $sawRoot) {
            Fail "ZIP archive does not contain $ArchiveRoot"
        }

        [System.IO.Directory]::CreateDirectory($DestinationDirectory) | Out-Null
        foreach ($plan in $plans) {
            $targetPath = $DestinationDirectory
            foreach ($segment in $plan.Segments) {
                $targetPath = Join-Path -Path $targetPath -ChildPath $segment
            }
            $targetPath = [System.IO.Path]::GetFullPath($targetPath)
            if (-not (Test-PathWithinRoot -Root $DestinationDirectory -Candidate $targetPath -AllowRoot)) {
                Fail "ZIP entry escapes staging directory: $($plan.Entry.FullName)"
            }

            if ($plan.IsDirectory) {
                if ([System.IO.File]::Exists($targetPath)) {
                    Fail "ZIP directory collides with a file: $($plan.Entry.FullName)"
                }
                [System.IO.Directory]::CreateDirectory($targetPath) | Out-Null
                continue
            }

            $parent = Split-Path -Parent $targetPath
            [System.IO.Directory]::CreateDirectory($parent) | Out-Null
            if ([System.IO.File]::Exists($targetPath) -or [System.IO.Directory]::Exists($targetPath)) {
                Fail "ZIP file collides with an existing path: $($plan.Entry.FullName)"
            }

            $input = $null
            $output = $null
            try {
                $input = $plan.Entry.Open()
                $output = [System.IO.FileStream]::new(
                    $targetPath,
                    [System.IO.FileMode]::CreateNew,
                    [System.IO.FileAccess]::Write,
                    [System.IO.FileShare]::None
                )
                $input.CopyTo($output)
            }
            finally {
                if ($null -ne $output) {
                    $output.Dispose()
                }
                if ($null -ne $input) {
                    $input.Dispose()
                }
            }
        }
    }
    finally {
        if ($null -ne $archive) {
            $archive.Dispose()
        }
    }
}

function Get-ExpectedChecksum {
    param(
        [Parameter(Mandatory = $true)][string]$ChecksumPath,
        [Parameter(Mandatory = $true)][string]$ArchiveName
    )

    $content = [System.IO.File]::ReadAllText($ChecksumPath, [System.Text.Encoding]::ASCII)
    $pattern = '\A(?<hash>[0-9A-Fa-f]{64})  ' + [System.Text.RegularExpressions.Regex]::Escape($ArchiveName) + '\r?\n?\z'
    $match = [System.Text.RegularExpressions.Regex]::Match(
        $content,
        $pattern,
        [System.Text.RegularExpressions.RegexOptions]::CultureInvariant
    )
    if (-not $match.Success) {
        Fail "invalid checksum file for $ArchiveName"
    }

    return $match.Groups['hash'].Value
}

function Assert-Checksum {
    param(
        [Parameter(Mandatory = $true)][string]$ArchivePath,
        [Parameter(Mandatory = $true)][string]$ChecksumPath,
        [Parameter(Mandatory = $true)][string]$ArchiveName
    )

    $expectedHash = Get-ExpectedChecksum -ChecksumPath $ChecksumPath -ArchiveName $ArchiveName
    $actualHash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash
    if (-not [string]::Equals($expectedHash, $actualHash, [System.StringComparison]::OrdinalIgnoreCase)) {
        Fail "checksum verification failed for $ArchiveName"
    }
}

function Copy-ReleaseAsset {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][bool]$AllowLocalFixture
    )

    if ($Source -match '^https://') {
        [System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor [System.Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri $Source -OutFile $Destination -UseBasicParsing -ErrorAction Stop
        return
    }

    if ($AllowLocalFixture -and $Source -match '^file://') {
        try {
            $uri = [System.Uri]::new($Source)
            if (-not $uri.IsFile -or -not $uri.IsAbsoluteUri) {
                Fail "invalid fixture URL: $Source"
            }
            Copy-Item -LiteralPath $uri.LocalPath -Destination $Destination -ErrorAction Stop
            return
        }
        catch {
            if ($_.Exception.Message -like 'invalid fixture URL:*') {
                throw
            }
            Fail "invalid fixture URL: $Source"
        }
    }

    if ($AllowLocalFixture -and [System.IO.Path]::IsPathRooted($Source)) {
        Copy-Item -LiteralPath $Source -Destination $Destination -ErrorAction Stop
        return
    }

    Fail "release downloads must use HTTPS: $Source"
}

function New-PrivateDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Parent,
        [Parameter(Mandatory = $true)][string]$Prefix
    )

    [System.IO.Directory]::CreateDirectory($Parent) | Out-Null
    for ($attempt = 0; $attempt -lt 10; $attempt++) {
        $path = Join-Path -Path $Parent -ChildPath ($Prefix + '-' + [System.Guid]::NewGuid().ToString('N'))
        try {
            [System.IO.Directory]::CreateDirectory($path) | Out-Null
            $item = Get-ExistingItem -Path $path
            if ($null -eq $item -or -not $item.PSIsContainer -or (Test-ReparsePoint -Item $item)) {
                Fail "cannot create private staging directory: $path"
            }
            return $item.FullName
        }
        catch {
            if ($attempt -eq 9) {
                throw
            }
        }
    }

    Fail "cannot create private staging directory under $Parent"
}

function Remove-NormalDirectoryTree {
    param([Parameter(Mandatory = $true)][string]$Path)

    $item = Get-ExistingItem -Path $Path
    if ($null -eq $item) {
        return
    }
    if (-not $item.PSIsContainer -or (Test-ReparsePoint -Item $item)) {
        Fail "refusing to remove non-directory or reparse point: $Path"
    }
    Assert-TreeHasNoReparsePoints -Root $Path
    [System.IO.Directory]::Delete($Path, $true)
}

function Remove-NormalDirectoryTreeBestEffort {
    param([string]$Path)

    if ([string]::IsNullOrEmpty($Path)) {
        return
    }
    try {
        Remove-NormalDirectoryTree -Path $Path
    }
    catch {
    }
}

function Assert-VersionOutput {
    param(
        [Parameter(Mandatory = $true)][string]$ExecutablePath,
        [Parameter(Mandatory = $true)][string]$ExpectedVersion,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $output = @(& $ExecutablePath '--version' 2>&1)
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        Fail "$Label --version failed with exit code $exitCode"
    }

    $rendered = [System.Collections.Generic.List[string]]::new()
    foreach ($line in $output) {
        [void]$rendered.Add([string]$line)
    }
    $actual = [string]::Join([Environment]::NewLine, $rendered.ToArray()).Trim()
    $expected = 'tessivum ' + $ExpectedVersion
    if (-not [string]::Equals($actual, $expected, [System.StringComparison]::Ordinal)) {
        Fail "$Label --version reported '$actual', expected '$expected'"
    }
}

function Assert-ReleaseLayout {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$ExpectedVersion
    )

    Assert-TreeHasNoReparsePoints -Root $Root
    $canonicalLauncher = Assert-RegularFile -Path (Join-Path -Path $Root -ChildPath 'bin\tessivum.cmd') -Label 'archive launcher bin\tessivum.cmd'
    $aliasLauncher = Assert-RegularFile -Path (Join-Path -Path $Root -ChildPath 'bin\tsv.cmd') -Label 'archive launcher bin\tsv.cmd'
    $binary = Assert-RegularFile -Path (Join-Path -Path $Root -ChildPath 'libexec\tessivum.exe') -Label 'archive binary libexec\tessivum.exe'

    Assert-VersionOutput -ExecutablePath $binary -ExpectedVersion $ExpectedVersion -Label 'archive binary'
    Assert-VersionOutput -ExecutablePath $canonicalLauncher -ExpectedVersion $ExpectedVersion -Label 'archive launcher bin\tessivum.cmd'
    Assert-VersionOutput -ExecutablePath $aliasLauncher -ExpectedVersion $ExpectedVersion -Label 'archive launcher bin\tsv.cmd'
}

function Get-InstallMarkerContent {
    param([Parameter(Mandatory = $true)][string]$InstalledVersion)

    return ('TESSIVUM-INSTALLED: 1' + "`r`n" + $InstalledVersion + "`r`n")
}

function Write-InstallMarker {
    param(
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string]$InstalledVersion
    )

    $markerPath = Join-Path -Path $Destination -ChildPath $script:InstallMarkerFileName
    if ($null -ne (Get-ExistingItem -Path $markerPath)) {
        Fail "archive contains reserved installer marker: $markerPath"
    }
    [System.IO.File]::WriteAllText(
        $markerPath,
        (Get-InstallMarkerContent -InstalledVersion $InstalledVersion),
        $script:Utf8NoBom
    )
}

function Test-ManagedVersionDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedVersion
    )

    try {
        if (-not (Test-VersionName -Value $ExpectedVersion)) {
            return $false
        }
        $item = Get-ExistingItem -Path $Path
        if ($null -eq $item -or -not $item.PSIsContainer -or (Test-ReparsePoint -Item $item)) {
            return $false
        }
        $markerPath = Join-Path -Path $Path -ChildPath $script:InstallMarkerFileName
        $marker = Get-ExistingItem -Path $markerPath
        if ($null -eq $marker -or $marker.PSIsContainer -or (Test-ReparsePoint -Item $marker)) {
            return $false
        }
        $content = [System.IO.File]::ReadAllText($markerPath, $script:Utf8NoBom)
        if (-not [string]::Equals(
            $content,
            (Get-InstallMarkerContent -InstalledVersion $ExpectedVersion),
            [System.StringComparison]::Ordinal
        )) {
            return $false
        }
        [void](Assert-RegularFile -Path (Join-Path -Path $Path -ChildPath 'bin\tessivum.cmd') -Label 'installed archive launcher')
        [void](Assert-RegularFile -Path (Join-Path -Path $Path -ChildPath 'bin\tsv.cmd') -Label 'installed archive alias launcher')
        [void](Assert-RegularFile -Path (Join-Path -Path $Path -ChildPath 'libexec\tessivum.exe') -Label 'installed archive binary')
        Assert-TreeHasNoReparsePoints -Root $Path
        return $true
    }
    catch {
        return $false
    }
}

function Assert-ManagedVersionDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedVersion
    )

    if (-not (Test-ManagedVersionDirectory -Path $Path -ExpectedVersion $ExpectedVersion)) {
        Fail "existing install is not a managed Tessivum version: $Path"
    }
    Assert-ReleaseLayout -Root $Path -ExpectedVersion $ExpectedVersion
}

function Get-PathStoreSnapshot {
    param([string]$PathStore)

    if (-not [string]::IsNullOrEmpty($PathStore)) {
        $item = Get-ExistingItem -Path $PathStore
        if ($null -eq $item) {
            return [PSCustomObject]@{
                Kind = 'file'
                Exists = $false
                Value = $null
            }
        }
        if ($item.PSIsContainer -or (Test-ReparsePoint -Item $item)) {
            Fail "PATH_STORE must be a regular file: $PathStore"
        }
        return [PSCustomObject]@{
            Kind = 'file'
            Exists = $true
            Value = [System.IO.File]::ReadAllText($PathStore, $script:Utf8NoBom)
        }
    }

    $value = [Environment]::GetEnvironmentVariable('Path', [EnvironmentVariableTarget]::User)
    return [PSCustomObject]@{
        Kind = 'user'
        Exists = ($null -ne $value)
        Value = $value
    }
}

function Write-AtomicTextFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Content
    )

    $parent = Split-Path -Parent $Path
    Ensure-NormalDirectory -Path $parent -Label 'text file parent' | Out-Null
    $temporaryPath = Join-Path -Path $parent -ChildPath ('.tessivum-write-' + [System.Guid]::NewGuid().ToString('N') + '.tmp')
    $backupPath = $null
    try {
        [System.IO.File]::WriteAllText($temporaryPath, $Content, $script:Utf8NoBom)
        $existing = Get-ExistingItem -Path $Path
        if ($null -eq $existing) {
            [System.IO.File]::Move($temporaryPath, $Path)
            return
        }
        if ($existing.PSIsContainer -or (Test-ReparsePoint -Item $existing)) {
            Fail "refusing to replace non-regular file: $Path"
        }

        $backupPath = Join-Path -Path $parent -ChildPath ('.tessivum-backup-' + [System.Guid]::NewGuid().ToString('N') + '.tmp')
        [System.IO.File]::Move($Path, $backupPath)
        try {
            [System.IO.File]::Move($temporaryPath, $Path)
        }
        catch {
            if ([System.IO.File]::Exists($backupPath) -and -not [System.IO.File]::Exists($Path)) {
                [System.IO.File]::Move($backupPath, $Path)
            }
            throw
        }
        [System.IO.File]::Delete($backupPath)
        $backupPath = $null
    }
    finally {
        if ([System.IO.File]::Exists($temporaryPath)) {
            [System.IO.File]::Delete($temporaryPath)
        }
        if ($null -ne $backupPath -and [System.IO.File]::Exists($backupPath)) {
            [System.IO.File]::Delete($backupPath)
        }
    }
}

function Set-PathStoreValue {
    param(
        [string]$PathStore,
        [AllowNull()][string]$Value
    )

    if (-not [string]::IsNullOrEmpty($PathStore)) {
        if ($null -eq $Value) {
            $item = Get-ExistingItem -Path $PathStore
            if ($null -ne $item) {
                if ($item.PSIsContainer -or (Test-ReparsePoint -Item $item)) {
                    Fail "PATH_STORE must be a regular file: $PathStore"
                }
                [System.IO.File]::Delete($PathStore)
            }
            return
        }
        Write-AtomicTextFile -Path $PathStore -Content $Value
        return
    }

    [Environment]::SetEnvironmentVariable('Path', $Value, [EnvironmentVariableTarget]::User)
}

function Restore-PathStoreSnapshot {
    param(
        [Parameter(Mandatory = $true)]$Snapshot,
        [string]$PathStore
    )

    if ($Snapshot.Kind -eq 'file' -and -not $Snapshot.Exists) {
        Set-PathStoreValue -PathStore $PathStore -Value $null
        return
    }
    Set-PathStoreValue -PathStore $PathStore -Value $Snapshot.Value
}

function Get-InstalledPathUpdate {
    param(
        [AllowNull()][string]$CurrentValue,
        [Parameter(Mandatory = $true)][string]$BinDirectory
    )

    $entries = if ($null -eq $CurrentValue) {
        @()
    }
    else {
        @($CurrentValue.Split([char[]]@(';'), [System.StringSplitOptions]::None))
    }
    $updatedEntries = [System.Collections.Generic.List[string]]::new()
    $found = $false
    foreach ($entry in $entries) {
        if ([string]::Equals($entry, $BinDirectory, [System.StringComparison]::OrdinalIgnoreCase)) {
            if (-not $found) {
                [void]$updatedEntries.Add($entry)
                $found = $true
            }
            continue
        }
        [void]$updatedEntries.Add($entry)
    }

    $added = -not $found
    if ($added) {
        [void]$updatedEntries.Add($BinDirectory)
    }
    $updated = [string]::Join(';', $updatedEntries.ToArray())
    return [PSCustomObject]@{
        Value = $updated
        Added = $added
        Changed = (-not [string]::Equals($CurrentValue, $updated, [System.StringComparison]::Ordinal))
    }
}

function Get-UninstalledPathUpdate {
    param(
        [AllowNull()][string]$CurrentValue,
        [Parameter(Mandatory = $true)][string]$BinDirectory
    )

    $entries = if ($null -eq $CurrentValue) {
        @()
    }
    else {
        @($CurrentValue.Split([char[]]@(';'), [System.StringSplitOptions]::None))
    }
    $updatedEntries = [System.Collections.Generic.List[string]]::new()
    foreach ($entry in $entries) {
        if (-not [string]::Equals($entry, $BinDirectory, [System.StringComparison]::OrdinalIgnoreCase)) {
            [void]$updatedEntries.Add($entry)
        }
    }
    $updated = [string]::Join(';', $updatedEntries.ToArray())
    return [PSCustomObject]@{
        Value = $updated
        Changed = (-not [string]::Equals($CurrentValue, $updated, [System.StringComparison]::Ordinal))
    }
}

function Get-PathOwnershipRecord {
    param(
        [Parameter(Mandatory = $true)][string]$OwnershipPath,
        [Parameter(Mandatory = $true)][string]$BinDirectory
    )

    $item = Get-ExistingItem -Path $OwnershipPath
    if ($null -eq $item) {
        return [PSCustomObject]@{
            Exists = $false
            Content = $null
        }
    }
    if ($item.PSIsContainer -or (Test-ReparsePoint -Item $item)) {
        Fail "refusing to replace unmanaged PATH ownership record: $OwnershipPath"
    }

    $content = [System.IO.File]::ReadAllText($OwnershipPath, $script:Utf8NoBom)
    $expected = 'TESSIVUM-PATH-OWNER: 1' + "`r`n" + $BinDirectory + "`r`n"
    if (-not [string]::Equals($content, $expected, [System.StringComparison]::Ordinal)) {
        Fail "refusing to replace unmanaged PATH ownership record: $OwnershipPath"
    }

    return [PSCustomObject]@{
        Exists = $true
        Content = $content
    }
}

function New-PathOwnershipContent {
    param([Parameter(Mandatory = $true)][string]$BinDirectory)

    return ('TESSIVUM-PATH-OWNER: 1' + "`r`n" + $BinDirectory + "`r`n")
}

function Enter-InstallerLock {
    param([Parameter(Mandatory = $true)][string]$InstallBase)

    Ensure-NormalDirectory -Path $InstallBase -Label 'installer base directory' | Out-Null
    $lockPath = Join-Path -Path $InstallBase -ChildPath '.tessivum-installer.lock'
    $existing = Get-ExistingItem -Path $lockPath
    if ($null -ne $existing -and ($existing.PSIsContainer -or (Test-ReparsePoint -Item $existing))) {
        Fail "installer lock path is unsafe: $lockPath"
    }

    try {
        $script:LockStream = [System.IO.File]::Open(
            $lockPath,
            [System.IO.FileMode]::OpenOrCreate,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None
        )
    }
    catch {
        Fail "another Tessivum installer operation is already running for $InstallBase"
    }
}

function Restore-Launchers {
    param([Parameter(Mandatory = $true)]$States)

    $problems = [System.Collections.Generic.List[string]]::new()
    for ($index = $States.Count - 1; $index -ge 0; $index--) {
        $state = $States[$index]
        try {
            if ($state.NewPublished) {
                $current = Get-ExistingItem -Path $state.Path
                if ($null -eq $current) {
                    $problems.Add("published launcher disappeared: $($state.Path)")
                }
                elseif ($current.PSIsContainer -or (Test-ReparsePoint -Item $current)) {
                    $problems.Add("published launcher became unsafe: $($state.Path)")
                }
                else {
                    $currentContent = [System.IO.File]::ReadAllText($state.Path, $script:Utf8NoBom)
                    if (-not [string]::Equals($currentContent, $state.Content, [System.StringComparison]::Ordinal)) {
                        $problems.Add("published launcher changed unexpectedly: $($state.Path)")
                    }
                    else {
                        [System.IO.File]::Delete($state.Path)
                    }
                }
            }

            if ($state.OldMoved) {
                if ([System.IO.File]::Exists($state.Backup)) {
                    if ($null -eq (Get-ExistingItem -Path $state.Path)) {
                        [System.IO.File]::Move($state.Backup, $state.Path)
                    }
                    else {
                        $problems.Add("cannot restore previous launcher: $($state.Path)")
                    }
                }
                elseif ($null -eq (Get-ExistingItem -Path $state.Path)) {
                    $problems.Add("previous launcher backup disappeared: $($state.Path)")
                }
            }
        }
        catch {
            $problems.Add($_.Exception.Message)
        }
    }

    return ,$problems
}

function Invoke-Install {
    param(
        [Parameter(Mandatory = $true)][string]$RequestedVersion,
        [Parameter(Mandatory = $true)][string]$Target,
        [Parameter(Mandatory = $true)][string]$InstallRoot,
        [Parameter(Mandatory = $true)][string]$BinDirectory,
        [Parameter(Mandatory = $true)][string]$InstallBase,
        [Parameter(Mandatory = $true)][bool]$TestMode,
        [AllowNull()][string]$FixtureUrl,
        [AllowNull()][string]$PathStore
    )

    Ensure-NormalDirectory -Path $InstallRoot -Label 'INSTALL_ROOT' | Out-Null
    Ensure-NormalDirectory -Path $BinDirectory -Label 'BIN_DIR' | Out-Null

    $archiveName = "tessivum-$RequestedVersion-$Target.zip"
    $archiveRoot = [System.IO.Path]::GetFileNameWithoutExtension($archiveName)
    $sourceUrl = if ([string]::IsNullOrEmpty($FixtureUrl)) {
        $script:ReleaseRepository + '/releases/download/v' + $RequestedVersion + '/' + $archiveName
    }
    else {
        $FixtureUrl
    }
    if (-not $TestMode -and -not $sourceUrl.StartsWith('https://', [System.StringComparison]::OrdinalIgnoreCase)) {
        Fail 'release downloads must use HTTPS'
    }

    $stagingDirectory = $null
    $launcherStagingDirectory = $null
    $destination = Join-Path -Path $InstallRoot -ChildPath $RequestedVersion
    $destinationCreated = $false
    $states = @()
    $pathSnapshot = $null
    $pathChanged = $false
    $ownershipCreated = $false
    $ownershipPath = Join-Path -Path $InstallBase -ChildPath $script:PathOwnerFileName

    try {
        $stagingDirectory = New-PrivateDirectory -Parent $InstallRoot -Prefix '.tessivum-install'
        $archivePath = Join-Path -Path $stagingDirectory -ChildPath $archiveName
        $checksumPath = $archivePath + '.sha256'
        Copy-ReleaseAsset -Source $sourceUrl -Destination $archivePath -AllowLocalFixture $TestMode
        Copy-ReleaseAsset -Source ($sourceUrl + '.sha256') -Destination $checksumPath -AllowLocalFixture $TestMode
        Assert-Checksum -ArchivePath $archivePath -ChecksumPath $checksumPath -ArchiveName $archiveName
        Expand-VerifiedZipArchive -ArchivePath $archivePath -DestinationDirectory $stagingDirectory -ArchiveRoot $archiveRoot

        $stagedRoot = Join-Path -Path $stagingDirectory -ChildPath $archiveRoot
        Assert-ReleaseLayout -Root $stagedRoot -ExpectedVersion $RequestedVersion
        if ($null -ne (Get-ExistingItem -Path (Join-Path -Path $stagedRoot -ChildPath $script:InstallMarkerFileName))) {
            Fail "archive contains reserved installer marker: $archiveRoot\$($script:InstallMarkerFileName)"
        }

        $existingDestination = Get-ExistingItem -Path $destination
        if ($null -ne $existingDestination) {
            if (-not $existingDestination.PSIsContainer -or (Test-ReparsePoint -Item $existingDestination)) {
                Fail "existing install is invalid: $destination"
            }
            Assert-ManagedVersionDirectory -Path $destination -ExpectedVersion $RequestedVersion
        }
        else {
            [System.IO.Directory]::Move($stagedRoot, $destination)
            $destinationCreated = $true
            Write-InstallMarker -Destination $destination -InstalledVersion $RequestedVersion
        }

        $launcherStagingDirectory = New-PrivateDirectory -Parent $BinDirectory -Prefix '.tessivum-launchers'
        foreach ($launcherName in @('tessivum', 'tsv')) {
            $launcherPath = Join-Path -Path $BinDirectory -ChildPath ($launcherName + '.cmd')
            [void](Assert-ReplaceableLauncher `
                -Path $launcherPath `
                -LauncherName $launcherName `
                -BinDirectory $BinDirectory `
                -InstallRoot $InstallRoot)

            $targetPath = Join-Path -Path $destination -ChildPath ('bin\' + $launcherName + '.cmd')
            $relativeTarget = Get-RelativePath -FromDirectory $BinDirectory -ToPath $targetPath
            $content = New-ManagedLauncherContent -RelativeTarget $relativeTarget
            $stagedPath = Join-Path -Path $launcherStagingDirectory -ChildPath ($launcherName + '.cmd')
            [System.IO.File]::WriteAllText($stagedPath, $content, $script:Utf8NoBom)
            $states += [PSCustomObject]@{
                Name = $launcherName
                Path = $launcherPath
                Staged = $stagedPath
                Backup = (Join-Path -Path $launcherStagingDirectory -ChildPath ('previous-' + $launcherName + '.cmd'))
                Content = $content
                OldMoved = $false
                NewPublished = $false
            }
        }

        $pathSnapshot = Get-PathStoreSnapshot -PathStore $PathStore
        $ownership = Get-PathOwnershipRecord -OwnershipPath $ownershipPath -BinDirectory $BinDirectory

        foreach ($state in $states) {
            if ($null -ne (Get-ExistingItem -Path $state.Path)) {
                [System.IO.File]::Move($state.Path, $state.Backup)
                $state.OldMoved = $true
            }
            [System.IO.File]::Move($state.Staged, $state.Path)
            $state.NewPublished = $true
        }

        $pathUpdate = Get-InstalledPathUpdate -CurrentValue $pathSnapshot.Value -BinDirectory $BinDirectory
        if ($pathUpdate.Changed) {
            Set-PathStoreValue -PathStore $PathStore -Value $pathUpdate.Value
            $pathChanged = $true
        }
        if ($pathUpdate.Added -and -not $ownership.Exists) {
            Write-AtomicTextFile -Path $ownershipPath -Content (New-PathOwnershipContent -BinDirectory $BinDirectory)
            $ownershipCreated = $true
        }

        # The transaction commits before staging directories are collected.
    }
    catch {
        $failure = $_
        $rollbackProblems = [System.Collections.Generic.List[string]]::new()

        if ($ownershipCreated) {
            try {
                $record = Get-PathOwnershipRecord -OwnershipPath $ownershipPath -BinDirectory $BinDirectory
                if ($record.Exists) {
                    [System.IO.File]::Delete($ownershipPath)
                }
            }
            catch {
                $rollbackProblems.Add($_.Exception.Message)
            }
        }
        if ($pathChanged -and $null -ne $pathSnapshot) {
            try {
                Restore-PathStoreSnapshot -Snapshot $pathSnapshot -PathStore $PathStore
            }
            catch {
                $rollbackProblems.Add($_.Exception.Message)
            }
        }

        $launcherProblems = Restore-Launchers -States $states
        foreach ($problem in $launcherProblems) {
            $rollbackProblems.Add($problem)
        }

        if ($destinationCreated) {
            try {
                Remove-NormalDirectoryTree -Path $destination
            }
            catch {
                $rollbackProblems.Add($_.Exception.Message)
            }
        }

        if ($rollbackProblems.Count -eq 0) {
            Remove-NormalDirectoryTreeBestEffort -Path $launcherStagingDirectory
            Remove-NormalDirectoryTreeBestEffort -Path $stagingDirectory
        }
        if ($rollbackProblems.Count -gt 0) {
            Fail ('installation failed: ' + $failure.Exception.Message + '; rollback failed: ' + [string]::Join('; ', $rollbackProblems.ToArray()))
        }
        throw
    }

    $cleanupProblems = [System.Collections.Generic.List[string]]::new()
    try {
        Remove-NormalDirectoryTree -Path $launcherStagingDirectory
        $launcherStagingDirectory = $null
    }
    catch {
        $cleanupProblems.Add('launcher staging cleanup failed: ' + $_.Exception.Message)
    }
    try {
        Remove-NormalDirectoryTree -Path $stagingDirectory
        $stagingDirectory = $null
    }
    catch {
        $cleanupProblems.Add('installation staging cleanup failed: ' + $_.Exception.Message)
    }
    if ($cleanupProblems.Count -gt 0) {
        Fail ('installation committed, but cleanup failed: ' + [string]::Join('; ', $cleanupProblems.ToArray()))
    }

    Write-Output "Installed Tessivum $RequestedVersion for $Target at $(Join-Path -Path $BinDirectory -ChildPath 'tessivum.cmd')"
    Write-Output 'Open a new terminal to use the updated User PATH entry.'
}

function Restore-UninstallState {
    param(
        [Parameter(Mandatory = $true)]$LauncherStates,
        [Parameter(Mandatory = $true)]$VersionStates,
        [Parameter(Mandatory = $true)][string]$OwnershipPath,
        [Parameter(Mandatory = $true)]$Ownership,
        [Parameter(Mandatory = $true)]$PathSnapshot,
        [Parameter(Mandatory = $true)][bool]$PathChanged,
        [Parameter(Mandatory = $true)][bool]$OwnershipRemoved,
        [AllowNull()][string]$PathStore
    )

    $problems = [System.Collections.Generic.List[string]]::new()
    if ($OwnershipRemoved) {
        try {
            Write-AtomicTextFile -Path $OwnershipPath -Content $Ownership.Content
        }
        catch {
            $problems.Add($_.Exception.Message)
        }
    }
    if ($PathChanged) {
        try {
            Restore-PathStoreSnapshot -Snapshot $PathSnapshot -PathStore $PathStore
        }
        catch {
            $problems.Add($_.Exception.Message)
        }
    }

    for ($index = $VersionStates.Count - 1; $index -ge 0; $index--) {
        $state = $VersionStates[$index]
        try {
            if ($state.Moved) {
                if ([System.IO.Directory]::Exists($state.Staged)) {
                    if (-not [System.IO.Directory]::Exists($state.Original)) {
                        [System.IO.Directory]::Move($state.Staged, $state.Original)
                    }
                    else {
                        $problems.Add("cannot restore version directory: $($state.Original)")
                    }
                }
                elseif (-not [System.IO.Directory]::Exists($state.Original)) {
                    $problems.Add("staged version directory disappeared: $($state.Original)")
                }
            }
        }
        catch {
            $problems.Add($_.Exception.Message)
        }
    }

    for ($index = $LauncherStates.Count - 1; $index -ge 0; $index--) {
        $state = $LauncherStates[$index]
        try {
            if ($state.Moved) {
                if ([System.IO.File]::Exists($state.Staged)) {
                    if ($null -eq (Get-ExistingItem -Path $state.Original)) {
                        [System.IO.File]::Move($state.Staged, $state.Original)
                    }
                    else {
                        $problems.Add("cannot restore launcher: $($state.Original)")
                    }
                }
                elseif ($null -eq (Get-ExistingItem -Path $state.Original)) {
                    $problems.Add("staged launcher disappeared: $($state.Original)")
                }
            }
        }
        catch {
            $problems.Add($_.Exception.Message)
        }
    }

    return ,$problems
}

function Remove-OwnedPathEntryOnly {
    param(
        [Parameter(Mandatory = $true)][string]$OwnershipPath,
        [Parameter(Mandatory = $true)][string]$BinDirectory,
        [AllowNull()][string]$PathStore
    )

    $ownership = Get-PathOwnershipRecord -OwnershipPath $OwnershipPath -BinDirectory $BinDirectory
    if (-not $ownership.Exists) {
        return
    }

    $snapshot = Get-PathStoreSnapshot -PathStore $PathStore
    $changed = $false
    try {
        $update = Get-UninstalledPathUpdate -CurrentValue $snapshot.Value -BinDirectory $BinDirectory
        if ($update.Changed) {
            Set-PathStoreValue -PathStore $PathStore -Value $update.Value
            $changed = $true
        }
        [System.IO.File]::Delete($OwnershipPath)
    }
    catch {
        if ($changed) {
            try {
                Restore-PathStoreSnapshot -Snapshot $snapshot -PathStore $PathStore
            }
            catch {
            }
        }
        throw
    }
}

function Invoke-Uninstall {
    param(
        [Parameter(Mandatory = $true)][string]$InstallRoot,
        [Parameter(Mandatory = $true)][string]$BinDirectory,
        [Parameter(Mandatory = $true)][string]$InstallBase,
        [AllowNull()][string]$PathStore
    )

    $canonicalPath = Join-Path -Path $BinDirectory -ChildPath 'tessivum.cmd'
    $aliasPath = Join-Path -Path $BinDirectory -ChildPath 'tsv.cmd'
    $ownershipPath = Join-Path -Path $InstallBase -ChildPath $script:PathOwnerFileName
    $canonicalItem = Get-ExistingItem -Path $canonicalPath
    $installRootItem = Get-ExistingItem -Path $InstallRoot

    if ($null -eq $canonicalItem) {
        if ($null -ne $installRootItem) {
            if (-not $installRootItem.PSIsContainer -or (Test-ReparsePoint -Item $installRootItem)) {
                Fail "refusing to remove invalid install root: $InstallRoot"
            }
            foreach ($child in ([System.IO.DirectoryInfo]::new($installRootItem.FullName)).EnumerateFileSystemInfos()) {
                Fail "refusing to remove $InstallRoot without a managed tessivum.cmd launcher"
            }
        }
        Remove-OwnedPathEntryOnly -OwnershipPath $ownershipPath -BinDirectory $BinDirectory -PathStore $PathStore
        Write-Output "Tessivum is already uninstalled from $InstallRoot"
        return
    }

    $canonical = Get-ManagedLauncherInfo `
        -Path $canonicalPath `
        -LauncherName 'tessivum' `
        -BinDirectory $BinDirectory `
        -InstallRoot $InstallRoot
    if ($null -eq $canonical) {
        Fail "refusing to remove unmanaged launcher: $canonicalPath"
    }
    if ($null -eq $installRootItem -or -not $installRootItem.PSIsContainer -or (Test-ReparsePoint -Item $installRootItem)) {
        Fail "refusing to remove invalid install root: $InstallRoot"
    }
    if (-not (Test-ManagedVersionDirectory -Path (Join-Path -Path $InstallRoot -ChildPath $canonical.Version) -ExpectedVersion $canonical.Version)) {
        Fail "refusing to remove version not validated by this installer: $($canonical.Version)"
    }

    $binItem = Assert-NormalDirectory -Path $BinDirectory -Label 'BIN_DIR'
    $managedLaunchers = @(
        [PSCustomObject]@{
            Name = 'tessivum'
            Original = $canonicalPath
        }
    )
    $aliasItem = Get-ExistingItem -Path $aliasPath
    if ($null -ne $aliasItem) {
        $alias = Get-ManagedLauncherInfo `
            -Path $aliasPath `
            -LauncherName 'tsv' `
            -BinDirectory $BinDirectory `
            -InstallRoot $InstallRoot
        if ($null -ne $alias) {
            $managedLaunchers += [PSCustomObject]@{
                Name = 'tsv'
                Original = $aliasPath
            }
        }
    }

    $managedVersions = [System.Collections.Generic.List[object]]::new()
    $rootDirectory = [System.IO.DirectoryInfo]::new($installRootItem.FullName)
    foreach ($child in $rootDirectory.EnumerateDirectories()) {
        if (-not (Test-VersionName -Value $child.Name)) {
            continue
        }
        if (Test-ManagedVersionDirectory -Path $child.FullName -ExpectedVersion $child.Name) {
            [void]$managedVersions.Add([PSCustomObject]@{
                Version = $child.Name
                Original = $child.FullName
            })
        }
    }
    if ($managedVersions.Count -eq 0) {
        Fail "refusing to remove $InstallRoot without managed version directories"
    }

    $launcherStage = $null
    $versionStage = $null
    $launcherStates = @()
    $versionStates = @()
    $ownership = $null
    $pathSnapshot = $null
    $pathChanged = $false
    $ownershipRemoved = $false

    try {
        $launcherStage = New-PrivateDirectory -Parent $binItem -Prefix '.tessivum-uninstall-launchers'
        $versionStage = New-PrivateDirectory -Parent $InstallBase -Prefix '.tessivum-uninstall-versions'
        foreach ($launcher in $managedLaunchers) {
            $staged = Join-Path -Path $launcherStage -ChildPath ($launcher.Name + '.cmd')
            [System.IO.File]::Move($launcher.Original, $staged)
            $launcherStates += [PSCustomObject]@{
                Original = $launcher.Original
                Staged = $staged
                Moved = $true
            }
        }
        foreach ($version in $managedVersions) {
            $staged = Join-Path -Path $versionStage -ChildPath $version.Version
            [System.IO.Directory]::Move($version.Original, $staged)
            $versionStates += [PSCustomObject]@{
                Original = $version.Original
                Staged = $staged
                Moved = $true
            }
        }

        $ownership = Get-PathOwnershipRecord -OwnershipPath $ownershipPath -BinDirectory $BinDirectory
        $pathSnapshot = Get-PathStoreSnapshot -PathStore $PathStore
        if ($ownership.Exists) {
            $pathUpdate = Get-UninstalledPathUpdate -CurrentValue $pathSnapshot.Value -BinDirectory $BinDirectory
            if ($pathUpdate.Changed) {
                Set-PathStoreValue -PathStore $PathStore -Value $pathUpdate.Value
                $pathChanged = $true
            }
            [System.IO.File]::Delete($ownershipPath)
            $ownershipRemoved = $true
        }

        # The transaction commits before staging directories are collected.
    }
    catch {
        $failure = $_
        if ($null -eq $ownership) {
            $ownership = [PSCustomObject]@{ Exists = $false; Content = $null }
        }
        if ($null -eq $pathSnapshot) {
            $pathSnapshot = [PSCustomObject]@{ Kind = 'file'; Exists = $false; Value = $null }
        }
        $rollbackProblems = Restore-UninstallState `
            -LauncherStates $launcherStates `
            -VersionStates $versionStates `
            -OwnershipPath $ownershipPath `
            -Ownership $ownership `
            -PathSnapshot $pathSnapshot `
            -PathChanged $pathChanged `
            -OwnershipRemoved $ownershipRemoved `
            -PathStore $PathStore
        if ($rollbackProblems.Count -eq 0) {
            Remove-NormalDirectoryTreeBestEffort -Path $launcherStage
            Remove-NormalDirectoryTreeBestEffort -Path $versionStage
        }
        if ($rollbackProblems.Count -gt 0) {
            Fail ('uninstall failed: ' + $failure.Exception.Message + '; rollback failed: ' + [string]::Join('; ', $rollbackProblems.ToArray()))
        }
        throw
    }

    $cleanupProblems = [System.Collections.Generic.List[string]]::new()
    try {
        Remove-NormalDirectoryTree -Path $launcherStage
        $launcherStage = $null
    }
    catch {
        $cleanupProblems.Add('launcher staging cleanup failed: ' + $_.Exception.Message)
    }
    try {
        Remove-NormalDirectoryTree -Path $versionStage
        $versionStage = $null
    }
    catch {
        $cleanupProblems.Add('version staging cleanup failed: ' + $_.Exception.Message)
    }
    if ($cleanupProblems.Count -gt 0) {
        Fail ('uninstall committed, but cleanup failed: ' + [string]::Join('; ', $cleanupProblems.ToArray()))
    }

    $dataPath = if ([string]::IsNullOrEmpty($env:USERPROFILE)) {
        '%USERPROFILE%\.tessivum'
    }
    else {
        Join-Path -Path $env:USERPROFILE -ChildPath '.tessivum'
    }
    Write-Output "Uninstalled Tessivum from $InstallRoot"
    Write-Output "User data was preserved at $dataPath"
    $quotedDataPath = $dataPath.Replace("'", "''")
    Write-Output "To remove it yourself: Remove-Item -LiteralPath '$quotedDataPath' -Recurse -Force"
}

try {
    if ($PSVersionTable.PSVersion.Major -lt 5) {
        Fail 'PowerShell 5.1 or later is required'
    }
    if ([Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
        Fail 'install.ps1 only supports Windows'
    }
    if ($Uninstall.IsPresent -and -not [string]::IsNullOrEmpty($Version)) {
        Show-Usage
    }

    $testModeValue = $env:TESSIVUM_INSTALLER_TEST
    if (-not [string]::IsNullOrEmpty($testModeValue) -and $testModeValue -ne '1') {
        Fail 'TESSIVUM_INSTALLER_TEST must be exactly 1 when set'
    }
    $testMode = ($testModeValue -eq '1')
    $testOverrideNames = @('FIXTURE_URL', 'INSTALL_ROOT', 'BIN_DIR', 'PATH_STORE')
    foreach ($name in $testOverrideNames) {
        $value = [Environment]::GetEnvironmentVariable($name, [EnvironmentVariableTarget]::Process)
        if (-not [string]::IsNullOrEmpty($value) -and -not $testMode) {
            Fail "$name is only allowed when TESSIVUM_INSTALLER_TEST=1"
        }
    }
    if (-not $testMode -and -not [string]::IsNullOrEmpty($env:REPOSITORY)) {
        Fail 'REPOSITORY is not configurable; release downloads are pinned to GitHub HTTPS releases'
    }

    $requestedVersion = if (-not $Uninstall.IsPresent -and -not [string]::IsNullOrEmpty($Version)) {
        $Version
    }
    elseif (-not $Uninstall.IsPresent -and -not [string]::IsNullOrEmpty($env:VERSION)) {
        $env:VERSION
    }
    else {
        $script:DefaultVersion
    }
    if (-not $Uninstall.IsPresent -and -not (Test-VersionName -Value $requestedVersion)) {
        Fail "invalid version: $requestedVersion"
    }

    $systemArchitecture = if (-not [string]::IsNullOrEmpty($env:PROCESSOR_ARCHITEW6432)) {
        $env:PROCESSOR_ARCHITEW6432
    }
    else {
        $env:PROCESSOR_ARCHITECTURE
    }
    if (-not [Environment]::Is64BitOperatingSystem -or $systemArchitecture -notmatch '^(AMD64|x86_64)$') {
        Fail "unsupported Windows architecture: $systemArchitecture"
    }

    $hasInstallRootOverride = $testMode -and -not [string]::IsNullOrEmpty($env:INSTALL_ROOT)
    $hasBinDirectoryOverride = $testMode -and -not [string]::IsNullOrEmpty($env:BIN_DIR)
    if ([string]::IsNullOrEmpty($env:LOCALAPPDATA) -and -not ($hasInstallRootOverride -and $hasBinDirectoryOverride)) {
        Fail 'LOCALAPPDATA is required unless INSTALL_ROOT and BIN_DIR are set in installer test mode'
    }
    $defaultBase = if ([string]::IsNullOrEmpty($env:LOCALAPPDATA)) {
        $null
    }
    else {
        Join-Path -Path $env:LOCALAPPDATA -ChildPath 'Tessivum'
    }
    $installRoot = if ($testMode -and -not [string]::IsNullOrEmpty($env:INSTALL_ROOT)) {
        Get-AbsolutePath -Value $env:INSTALL_ROOT -Label 'INSTALL_ROOT'
    }
    else {
        Get-AbsolutePath -Value (Join-Path -Path $defaultBase -ChildPath 'versions') -Label 'install root'
    }
    $binDirectory = if ($testMode -and -not [string]::IsNullOrEmpty($env:BIN_DIR)) {
        Get-AbsolutePath -Value $env:BIN_DIR -Label 'BIN_DIR'
    }
    else {
        Get-AbsolutePath -Value (Join-Path -Path $defaultBase -ChildPath 'bin') -Label 'bin directory'
    }
    Assert-NotFilesystemRoot -Path $installRoot -Label 'INSTALL_ROOT'
    Assert-NotFilesystemRoot -Path $binDirectory -Label 'BIN_DIR'
    if (-not [string]::Equals(
        [System.IO.Path]::GetPathRoot($installRoot),
        [System.IO.Path]::GetPathRoot($binDirectory),
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        Fail 'INSTALL_ROOT and BIN_DIR must be on the same volume'
    }

    $installBase = Get-AbsolutePath -Value (Split-Path -Parent $installRoot) -Label 'installer base directory'
    Assert-NotFilesystemRoot -Path $installBase -Label 'installer base directory'
    $pathStore = if ($testMode -and -not [string]::IsNullOrEmpty($env:PATH_STORE)) {
        Get-AbsolutePath -Value $env:PATH_STORE -Label 'PATH_STORE'
    }
    else {
        $null
    }
    $fixtureUrl = if ($testMode -and -not [string]::IsNullOrEmpty($env:FIXTURE_URL)) {
        $env:FIXTURE_URL
    }
    else {
        $null
    }

    Enter-InstallerLock -InstallBase $installBase
    if ($Uninstall.IsPresent) {
        Invoke-Uninstall `
            -InstallRoot $installRoot `
            -BinDirectory $binDirectory `
            -InstallBase $installBase `
            -PathStore $pathStore
    }
    else {
        Invoke-Install `
            -RequestedVersion $requestedVersion `
            -Target $script:Target `
            -InstallRoot $installRoot `
            -BinDirectory $binDirectory `
            -InstallBase $installBase `
            -TestMode $testMode `
            -FixtureUrl $fixtureUrl `
            -PathStore $pathStore
    }
}
catch {
    [Console]::Error.WriteLine('install.ps1: ' + $_.Exception.Message)
    [Console]::Error.WriteLine($_.ScriptStackTrace)
    exit 1
}
finally {
    if ($null -ne $script:LockStream) {
        $script:LockStream.Dispose()
        $script:LockStream = $null
    }
}
