[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$ArchivePath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$script:Target = 'x86_64-pc-windows-msvc'
$script:WorkDirectory = $null
$script:InstallerPath = Join-Path -Path (Split-Path -Parent $PSScriptRoot) -ChildPath 'install.ps1'
$script:PowerShellHost = $null

function Fail {
    param([Parameter(Mandatory = $true)][string]$Message)

    throw ('test_install.ps1: ' + $Message)
}

function Assert-True {
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if (-not $Condition) {
        Fail $Message
    }
}

function Assert-Equal {
    param(
        [AllowNull()]$Expected,
        [AllowNull()]$Actual,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if ($Expected -is [string] -or $Actual -is [string]) {
        if (-not [string]::Equals([string]$Expected, [string]$Actual, [System.StringComparison]::Ordinal)) {
            Fail ($Message + "; expected '$Expected', got '$Actual'")
        }
        return
    }
    if ($Expected -ne $Actual) {
        Fail ($Message + "; expected '$Expected', got '$Actual'")
    }
}

function Ensure-ZipSupport {
    if ($null -eq ('System.IO.Compression.ZipFile' -as [type])) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
    }
}

function Get-AbsolutePath {
    param([Parameter(Mandatory = $true)][string]$Path)

    try {
        return [System.IO.Path]::GetFullPath($Path)
    }
    catch {
        Fail "invalid path: $Path"
    }
}

function Get-FileUri {
    param([Parameter(Mandatory = $true)][string]$Path)

    return [System.Uri]::new((Get-AbsolutePath -Path $Path)).AbsoluteUri
}

function Get-ResultText {
    param([Parameter(Mandatory = $true)]$Result)

    $lines = [System.Collections.Generic.List[string]]::new()
    foreach ($line in $Result.Output) {
        [void]$lines.Add([string]$line)
    }
    return [string]::Join([Environment]::NewLine, $lines.ToArray())
}

function Assert-Succeeded {
    param(
        [Parameter(Mandatory = $true)]$Result,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ($Result.ExitCode -ne 0) {
        Fail ($Label + ' failed with exit code ' + $Result.ExitCode + ': ' + (Get-ResultText -Result $Result))
    }
}

function Assert-Failed {
    param(
        [Parameter(Mandatory = $true)]$Result,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ($Result.ExitCode -eq 0) {
        Fail ($Label + ' unexpectedly succeeded: ' + (Get-ResultText -Result $Result))
    }
}

function Get-ReleaseArchiveInfo {
    param([Parameter(Mandatory = $true)][string]$Path)

    $fileName = [System.IO.Path]::GetFileName($Path)
    $pattern = '^tessivum-(?<version>[0-9A-Za-z][0-9A-Za-z.-]*)-' + [System.Text.RegularExpressions.Regex]::Escape($script:Target) + '\.zip$'
    $match = [System.Text.RegularExpressions.Regex]::Match($fileName, $pattern, [System.Text.RegularExpressions.RegexOptions]::CultureInvariant)
    Assert-True -Condition $match.Success -Message "release archive has an unexpected name: $fileName"
    $version = $match.Groups['version'].Value
    Assert-True -Condition ($version.IndexOf('..', [System.StringComparison]::Ordinal) -lt 0) -Message "release archive has an unsafe version: $version"
    $root = 'tessivum-' + $version + '-' + $script:Target

    Ensure-ZipSupport
    $archive = $null
    try {
        $archive = [System.IO.Compression.ZipFile]::OpenRead($Path)
        $canonicalLauncher = $false
        $aliasLauncher = $false
        $binary = $false
        foreach ($entry in $archive.Entries) {
            $name = $entry.FullName
            Assert-True -Condition (
                $name -ceq $root -or $name -ceq ($root + '/') -or $name.StartsWith($root + '/', [System.StringComparison]::Ordinal)
            ) -Message "release archive has an unexpected root entry: $name"
            if ($name -ceq ($root + '/bin/tessivum.cmd')) {
                $canonicalLauncher = $true
            }
            elseif ($name -ceq ($root + '/bin/tsv.cmd')) {
                $aliasLauncher = $true
            }
            elseif ($name -ceq ($root + '/libexec/tessivum.exe')) {
                $binary = $true
            }
        }
        Assert-True -Condition $canonicalLauncher -Message 'release archive is missing bin/tessivum.cmd'
        Assert-True -Condition $aliasLauncher -Message 'release archive is missing bin/tsv.cmd'
        Assert-True -Condition $binary -Message 'release archive is missing libexec/tessivum.exe'
    }
    finally {
        if ($null -ne $archive) {
            $archive.Dispose()
        }
    }

    return [PSCustomObject]@{
        Version = $version
        Root = $root
        ArchiveName = $fileName
    }
}

function Write-ChecksumFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedArchiveName
    )

    $hash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText($Path + '.sha256', ($hash + '  ' + $ExpectedArchiveName + "`r`n"), $script:Utf8NoBom)
}

function New-MinimalZipArchive {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][object[]]$Entries,
        [Parameter(Mandatory = $true)][string]$ExpectedArchiveName
    )

    Ensure-ZipSupport
    $archive = $null
    try {
        $archive = [System.IO.Compression.ZipFile]::Open($Path, [System.IO.Compression.ZipArchiveMode]::Create)
        foreach ($specification in $Entries) {
            $entry = $archive.CreateEntry($specification.Name, [System.IO.Compression.CompressionLevel]::NoCompression)
            if ($null -ne $specification.ExternalAttributes) {
                $entry.ExternalAttributes = [int]$specification.ExternalAttributes
            }
            if ($null -eq $specification.Content) {
                continue
            }
            [byte[]]$content = $null
            if ($specification.Content -is [byte[]]) {
                $content = $specification.Content
            }
            else {
                $content = $script:Utf8NoBom.GetBytes([string]$specification.Content)
            }
            $stream = $null
            try {
                $stream = $entry.Open()
                $stream.Write($content, 0, $content.Length)
            }
            finally {
                if ($null -ne $stream) {
                    $stream.Dispose()
                }
            }
        }
    }
    finally {
        if ($null -ne $archive) {
            $archive.Dispose()
        }
    }
    Write-ChecksumFile -Path $Path -ExpectedArchiveName $ExpectedArchiveName
}

function Replace-BytesInPlace {
    param(
        [Parameter(Mandatory = $true)][byte[]]$Data,
        [Parameter(Mandatory = $true)][byte[]]$Needle,
        [Parameter(Mandatory = $true)][byte[]]$Replacement
    )

    Assert-Equal -Expected $Needle.Length -Actual $Replacement.Length -Message 'version patch must preserve executable byte length'
    $replaced = 0
    for ($index = 0; $index -le ($Data.Length - $Needle.Length); $index++) {
        $matches = $true
        for ($offset = 0; $offset -lt $Needle.Length; $offset++) {
            if ($Data[$index + $offset] -ne $Needle[$offset]) {
                $matches = $false
                break
            }
        }
        if ($matches) {
            [System.Array]::Copy($Replacement, 0, $Data, $index, $Replacement.Length)
            $replaced++
            $index += $Needle.Length - 1
        }
    }
    return $replaced
}

function New-VersionVariantArchive {
    param(
        [Parameter(Mandatory = $true)][string]$SourceArchive,
        [Parameter(Mandatory = $true)][string]$SourceRoot,
        [Parameter(Mandatory = $true)][string]$SourceVersion,
        [Parameter(Mandatory = $true)][string]$VariantVersion,
        [Parameter(Mandatory = $true)][string]$DestinationArchive
    )

    Ensure-ZipSupport
    $variantRoot = 'tessivum-' + $VariantVersion + '-' + $script:Target
    $source = $null
    $destination = $null
    $patchedOccurrences = 0
    try {
        $source = [System.IO.Compression.ZipFile]::OpenRead($SourceArchive)
        $destination = [System.IO.Compression.ZipFile]::Open($DestinationArchive, [System.IO.Compression.ZipArchiveMode]::Create)
        foreach ($entry in $source.Entries) {
            $name = $entry.FullName
            if ($name -ceq $SourceRoot) {
                $newName = $variantRoot
            }
            elseif ($name.StartsWith($SourceRoot + '/', [System.StringComparison]::Ordinal)) {
                $newName = $variantRoot + $name.Substring($SourceRoot.Length)
            }
            else {
                Fail "cannot make a variant from an archive with an unexpected entry: $name"
            }

            $newEntry = $destination.CreateEntry($newName, [System.IO.Compression.CompressionLevel]::Optimal)
            $newEntry.LastWriteTime = $entry.LastWriteTime
            $newEntry.ExternalAttributes = $entry.ExternalAttributes
            if ($name.EndsWith('/')) {
                continue
            }

            $input = $null
            $output = $null
            try {
                $input = $entry.Open()
                $output = $newEntry.Open()
                if ($name -ceq ($SourceRoot + '/libexec/tessivum.exe')) {
                    $memory = [System.IO.MemoryStream]::new()
                    try {
                        $input.CopyTo($memory)
                        $bytes = $memory.ToArray()
                    }
                    finally {
                        $memory.Dispose()
                    }
                    $patchedOccurrences += (Replace-BytesInPlace -Data $bytes -Needle $script:Utf8NoBom.GetBytes($SourceVersion) -Replacement $script:Utf8NoBom.GetBytes($VariantVersion))
                    $output.Write($bytes, 0, $bytes.Length)
                }
                else {
                    $input.CopyTo($output)
                }
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
        if ($null -ne $destination) {
            $destination.Dispose()
        }
        if ($null -ne $source) {
            $source.Dispose()
        }
    }

    Assert-True -Condition ($patchedOccurrences -gt 0) -Message "the real packaged tessivum.exe did not contain its version string for the versioned fixture"
    Write-ChecksumFile -Path $DestinationArchive -ExpectedArchiveName ([System.IO.Path]::GetFileName($DestinationArchive))
}

function Get-VersionVariants {
    param([Parameter(Mandatory = $true)][string]$SourceVersion)

    $match = [regex]::Match($SourceVersion, '^(.*?)([0-9]+)$')
    Assert-True -Condition $match.Success -Message "release version must end in a numeric component: $SourceVersion"
    $number = [System.Numerics.BigInteger]::Parse($match.Groups[2].Value)
    Assert-True -Condition ($number -gt 0) -Message 'upgrade fixtures require a positive final version component'
    $prefix = $match.Groups[1].Value
    $oldVersion = $prefix + ($number - 1).ToString()
    $newVersion = $prefix + ($number + 1).ToString()
    # Binary patch fixtures must preserve the embedded version's byte length.
    Assert-True -Condition ($oldVersion.Length -eq $SourceVersion.Length -and $newVersion.Length -eq $SourceVersion.Length) -Message 'upgrade fixtures require adjacent versions with the same byte length'
    return [PSCustomObject]@{
        Old = $oldVersion
        New = $newVersion
    }
}

function New-TestPrefix {
    param([Parameter(Mandatory = $true)][string]$Name)

    $base = Join-Path -Path $script:WorkDirectory -ChildPath $Name
    $tessivumBase = Join-Path -Path $base -ChildPath 'Tessivum'
    $configuration = [PSCustomObject]@{
        Base = $base
        InstallRoot = Join-Path -Path $tessivumBase -ChildPath 'versions'
        BinDirectory = Join-Path -Path $tessivumBase -ChildPath 'bin'
        PathStore = Join-Path -Path $base -ChildPath 'user-path.txt'
        Home = Join-Path -Path $base -ChildPath 'user-home'
        LocalAppData = Join-Path -Path $base -ChildPath 'local-appdata'
    }
    [System.IO.Directory]::CreateDirectory($configuration.Base) | Out-Null
    [System.IO.Directory]::CreateDirectory($configuration.Home) | Out-Null
    [System.IO.File]::WriteAllText($configuration.PathStore, '', $script:Utf8NoBom)
    return $configuration
}

function Set-TestPathStore {
    param(
        [Parameter(Mandatory = $true)]$Configuration,
        [AllowEmptyString()][string]$Value
    )

    [System.IO.File]::WriteAllText($Configuration.PathStore, $Value, $script:Utf8NoBom)
}

function Get-TestPathEntries {
    param([Parameter(Mandatory = $true)]$Configuration)

    [string[]]$entries = @()
    if ([System.IO.File]::Exists($Configuration.PathStore)) {
        $value = [System.IO.File]::ReadAllText($Configuration.PathStore, $script:Utf8NoBom)
        if (-not [string]::IsNullOrEmpty($value)) {
            $entries = $value.Split([char[]]@(';'), [System.StringSplitOptions]::None)
        }
    }
    return ,$entries
}

function Get-PathEntryCount {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyString()][AllowEmptyCollection()][string[]]$Entries,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $count = 0
    foreach ($entry in $Entries) {
        if ([string]::Equals($entry, $Path, [System.StringComparison]::OrdinalIgnoreCase)) {
            $count++
        }
    }
    return $count
}

function Invoke-InstallerProcess {
    param(
        [Parameter(Mandatory = $true)][hashtable]$Overrides,
        [Parameter(Mandatory = $true)][string[]]$InstallerArguments
    )

    $controlledNames = @(
        'TESSIVUM_INSTALLER_TEST',
        'FIXTURE_URL',
        'INSTALL_ROOT',
        'BIN_DIR',
        'PATH_STORE',
        'VERSION',
        'REPOSITORY',
        'USERPROFILE',
        'LOCALAPPDATA',
        'TESSIVUM_HOME',
        'TESSIVUM_COMPAT_HOST',
        'TESSIVUM_HOST_MODULE_ROOT',
        'TESSIVUM_MARKET_TARBALL',
        'TESSIVUM_MARKET_SHA256_FILE',
        'TESSIVUM_MARKET_SOURCE_FILE',
        'CORDIS_VENDOR_ROOT'
    )
    $original = @{}
    foreach ($name in $controlledNames) {
        $original[$name] = [Environment]::GetEnvironmentVariable($name, [EnvironmentVariableTarget]::Process)
        [Environment]::SetEnvironmentVariable($name, $null, [EnvironmentVariableTarget]::Process)
    }

    try {
        foreach ($name in $Overrides.Keys) {
            [Environment]::SetEnvironmentVariable($name, [string]$Overrides[$name], [EnvironmentVariableTarget]::Process)
        }
        $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $script:InstallerPath)
        $arguments += $InstallerArguments
        $output = @(& $script:PowerShellHost @arguments 2>&1)
        return [PSCustomObject]@{
            ExitCode = $LASTEXITCODE
            Output = $output
        }
    }
    finally {
        foreach ($name in $controlledNames) {
            [Environment]::SetEnvironmentVariable($name, $original[$name], [EnvironmentVariableTarget]::Process)
        }
    }
}

function Invoke-TestInstall {
    param(
        [Parameter(Mandatory = $true)]$Configuration,
        [Parameter(Mandatory = $true)][string]$FixtureArchive,
        [Parameter(Mandatory = $true)][string]$Version
    )

    $overrides = @{
        TESSIVUM_INSTALLER_TEST = '1'
        FIXTURE_URL = Get-FileUri -Path $FixtureArchive
        INSTALL_ROOT = $Configuration.InstallRoot
        BIN_DIR = $Configuration.BinDirectory
        PATH_STORE = $Configuration.PathStore
        USERPROFILE = $Configuration.Home
        LOCALAPPDATA = $Configuration.LocalAppData
    }
    return Invoke-InstallerProcess -Overrides $overrides -InstallerArguments @($Version)
}

function Invoke-TestUninstall {
    param([Parameter(Mandatory = $true)]$Configuration)

    $overrides = @{
        TESSIVUM_INSTALLER_TEST = '1'
        INSTALL_ROOT = $Configuration.InstallRoot
        BIN_DIR = $Configuration.BinDirectory
        PATH_STORE = $Configuration.PathStore
        USERPROFILE = $Configuration.Home
        LOCALAPPDATA = $Configuration.LocalAppData
    }
    return Invoke-InstallerProcess -Overrides $overrides -InstallerArguments @('-Uninstall')
}

function Invoke-UngatedFixtureInstall {
    param(
        [Parameter(Mandatory = $true)]$Configuration,
        [Parameter(Mandatory = $true)][string]$FixtureArchive,
        [Parameter(Mandatory = $true)][string]$Version
    )

    $overrides = @{
        FIXTURE_URL = Get-FileUri -Path $FixtureArchive
        USERPROFILE = $Configuration.Home
        LOCALAPPDATA = $Configuration.LocalAppData
    }
    return Invoke-InstallerProcess -Overrides $overrides -InstallerArguments @($Version)
}

function Get-LauncherVersion {
    param(
        [Parameter(Mandatory = $true)]$Configuration,
        [Parameter(Mandatory = $true)][string]$LauncherPath,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $environmentNames = @('USERPROFILE', 'LOCALAPPDATA', 'TESSIVUM_HOME')
    $previous = @{}
    foreach ($name in $environmentNames) {
        $previous[$name] = [Environment]::GetEnvironmentVariable($name, [EnvironmentVariableTarget]::Process)
        [Environment]::SetEnvironmentVariable($name, $null, [EnvironmentVariableTarget]::Process)
    }
    try {
        [Environment]::SetEnvironmentVariable('USERPROFILE', $Configuration.Home, [EnvironmentVariableTarget]::Process)
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $Configuration.LocalAppData, [EnvironmentVariableTarget]::Process)
        $output = @(& $LauncherPath '--version' 2>&1)
        $exitCode = $LASTEXITCODE
        Assert-Equal -Expected 0 -Actual $exitCode -Message "$Label --version exit code"
        $lines = [System.Collections.Generic.List[string]]::new()
        foreach ($line in $output) {
            [void]$lines.Add([string]$line)
        }
        return [string]::Join([Environment]::NewLine, $lines.ToArray()).Trim()
    }
    finally {
        foreach ($name in $environmentNames) {
            [Environment]::SetEnvironmentVariable($name, $previous[$name], [EnvironmentVariableTarget]::Process)
        }
    }
}

function Assert-StableLaunchers {
    param(
        [Parameter(Mandatory = $true)]$Configuration,
        [Parameter(Mandatory = $true)][string]$ExpectedVersion,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $expectedOutput = 'tessivum ' + $ExpectedVersion
    $canonical = Join-Path -Path $Configuration.BinDirectory -ChildPath 'tessivum.cmd'
    $alias = Join-Path -Path $Configuration.BinDirectory -ChildPath 'tsv.cmd'
    Assert-True -Condition ([System.IO.File]::Exists($canonical)) -Message "$Label is missing stable tessivum.cmd"
    Assert-True -Condition ([System.IO.File]::Exists($alias)) -Message "$Label is missing stable tsv.cmd"
    Assert-Equal -Expected $expectedOutput -Actual (Get-LauncherVersion -Configuration $Configuration -LauncherPath $canonical -Label "$Label tessivum") -Message "$Label tessivum version"
    Assert-Equal -Expected $expectedOutput -Actual (Get-LauncherVersion -Configuration $Configuration -LauncherPath $alias -Label "$Label tsv") -Message "$Label tsv version"
}

function Assert-NoPartialInstall {
    param(
        [Parameter(Mandatory = $true)]$Configuration,
        [Parameter(Mandatory = $true)][string]$Version,
        [string]$EscapedPath
    )

    $destination = Join-Path -Path $Configuration.InstallRoot -ChildPath $Version
    Assert-True -Condition (-not (Test-Path -LiteralPath $destination)) -Message "failed install left a version directory: $destination"
    foreach ($root in @($Configuration.InstallRoot, $Configuration.BinDirectory)) {
        if (Test-Path -LiteralPath $root) {
            foreach ($item in @(Get-ChildItem -LiteralPath $root -Force -ErrorAction Stop)) {
                Assert-True -Condition (
                    -not $item.Name.StartsWith('.tessivum-install-', [System.StringComparison]::Ordinal) -and
                    -not $item.Name.StartsWith('.tessivum-launchers-', [System.StringComparison]::Ordinal)
                ) -Message "failed install left staging state: $($item.FullName)"
            }
        }
    }
    if (-not [string]::IsNullOrEmpty($EscapedPath)) {
        Assert-True -Condition (-not (Test-Path -LiteralPath $EscapedPath)) -Message "ZIP Slip created $EscapedPath"
    }
}

function Get-PrivateStagingDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Parent,
        [Parameter(Mandatory = $true)][string]$Prefix,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $directories = @([System.IO.Directory]::EnumerateDirectories($Parent, ($Prefix + '-*')))
    Assert-Equal -Expected 1 -Actual $directories.Count -Message "$Label did not retain exactly one staging directory"
    return $directories[0]
}

function Clear-ReadOnlyFiles {
    param([Parameter(Mandatory = $true)][string]$Root)

    if (-not [System.IO.Directory]::Exists($Root)) {
        return
    }

    $readOnly = [int][System.IO.FileAttributes]::ReadOnly
    foreach ($path in [System.IO.Directory]::EnumerateFiles($Root, '*', [System.IO.SearchOption]::AllDirectories)) {
        $attributes = [System.IO.File]::GetAttributes($path)
        if (([int]$attributes -band $readOnly) -ne 0) {
            $updatedAttributeValue = ([int]$attributes -band (-bnot $readOnly))
            [System.IO.File]::SetAttributes($path, [System.IO.FileAttributes]$updatedAttributeValue)
        }
    }
}

try {
    if ($PSVersionTable.PSVersion.Major -lt 5) {
        Fail 'PowerShell 5.1 or later is required'
    }
    if ([Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
        Fail 'this test only supports Windows'
    }

    $archive = Get-AbsolutePath -Path $ArchivePath
    Assert-True -Condition ([System.IO.File]::Exists($archive)) -Message "archive does not exist: $archive"
    Assert-True -Condition ([System.IO.File]::Exists($archive + '.sha256')) -Message "archive checksum does not exist: $archive.sha256"
    Assert-True -Condition ([System.IO.File]::Exists($script:InstallerPath)) -Message "installer does not exist: $script:InstallerPath"

    $candidateHost = if ($PSVersionTable.PSEdition -eq 'Core') {
        Join-Path -Path $PSHOME -ChildPath 'pwsh.exe'
    }
    else {
        Join-Path -Path $PSHOME -ChildPath 'powershell.exe'
    }
    if ([System.IO.File]::Exists($candidateHost)) {
        $script:PowerShellHost = $candidateHost
    }
    else {
        $script:PowerShellHost = (Get-Process -Id $PID).Path
    }
    Assert-True -Condition ([System.IO.File]::Exists($script:PowerShellHost)) -Message 'cannot locate the current PowerShell executable'

    $release = Get-ReleaseArchiveInfo -Path $archive
    $variants = Get-VersionVariants -SourceVersion $release.Version
    $script:WorkDirectory = Join-Path -Path ([System.IO.Path]::GetTempPath()) -ChildPath ('tessivum installer test ' + [System.Guid]::NewGuid().ToString('N'))
    [System.IO.Directory]::CreateDirectory($script:WorkDirectory) | Out-Null
    $fixtureDirectory = Join-Path -Path $script:WorkDirectory -ChildPath 'fixtures'
    [System.IO.Directory]::CreateDirectory($fixtureDirectory) | Out-Null

    $oldArchive = Join-Path -Path $fixtureDirectory -ChildPath ('tessivum-' + $variants.Old + '-' + $script:Target + '.zip')
    $newArchive = Join-Path -Path $fixtureDirectory -ChildPath ('tessivum-' + $variants.New + '-' + $script:Target + '.zip')
    New-VersionVariantArchive -SourceArchive $archive -SourceRoot $release.Root -SourceVersion $release.Version -VariantVersion $variants.Old -DestinationArchive $oldArchive
    New-VersionVariantArchive -SourceArchive $archive -SourceRoot $release.Root -SourceVersion $release.Version -VariantVersion $variants.New -DestinationArchive $newArchive

    $gate = New-TestPrefix -Name 'fixture gate'
    $gateResult = Invoke-UngatedFixtureInstall -Configuration $gate -FixtureArchive $archive -Version $release.Version
    Assert-Failed -Result $gateResult -Label 'fixture override without explicit test mode'
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path -Path $gate.LocalAppData -ChildPath 'Tessivum'))) -Message 'ungated fixture override created a default install root'

    $actual = New-TestPrefix -Name 'real package 空格 中文'
    $unrelatedOne = Join-Path -Path $actual.Base -ChildPath 'unrelated-one'
    $unrelatedTwo = Join-Path -Path $actual.Base -ChildPath 'unrelated-two'
    Set-TestPathStore -Configuration $actual -Value ($unrelatedOne + ';' + $unrelatedTwo)
    $freshResult = Invoke-TestInstall -Configuration $actual -FixtureArchive $archive -Version $release.Version
    Assert-Succeeded -Result $freshResult -Label 'fresh install of the real packaged executable'
    Assert-True -Condition ((Get-ResultText -Result $freshResult).Contains('for ' + $script:Target)) -Message 'fresh install did not select the x86-64 Windows target'
    Assert-StableLaunchers -Configuration $actual -ExpectedVersion $release.Version -Label 'fresh real package install'
    $reinstallResult = Invoke-TestInstall -Configuration $actual -FixtureArchive $archive -Version $release.Version
    Assert-Succeeded -Result $reinstallResult -Label 'same-version reinstall of the real packaged executable'
    Assert-StableLaunchers -Configuration $actual -ExpectedVersion $release.Version -Label 'same-version real package reinstall'
    $lockPath = Join-Path -Path (Split-Path -Parent $actual.InstallRoot) -ChildPath '.tessivum-installer.lock'
    $exclusiveInstallerLock = [System.IO.File]::Open($lockPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
    try {
        $concurrentInstall = Invoke-TestInstall -Configuration $actual -FixtureArchive $archive -Version $release.Version
    }
    finally {
        $exclusiveInstallerLock.Dispose()
    }
    Assert-Failed -Result $concurrentInstall -Label 'concurrent installer operation'
    Assert-StableLaunchers -Configuration $actual -ExpectedVersion $release.Version -Label 'concurrent installer rejection'

    $actualEntries = Get-TestPathEntries -Configuration $actual
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $actualEntries -Path $actual.BinDirectory) -Message 'installer did not deduplicate its User PATH entry'
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $actualEntries -Path $unrelatedOne) -Message 'installer changed the first unrelated User PATH entry'
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $actualEntries -Path $unrelatedTwo) -Message 'installer changed the second unrelated User PATH entry'

    $preowned = New-TestPrefix -Name 'preowned path'
    $preownedUnrelated = Join-Path -Path $preowned.Base -ChildPath 'unrelated'
    Set-TestPathStore -Configuration $preowned -Value ($preownedUnrelated + ';' + $preowned.BinDirectory + ';' + $preowned.BinDirectory)
    Assert-Succeeded -Result (Invoke-TestInstall -Configuration $preowned -FixtureArchive $archive -Version $release.Version) -Label 'install with a preexisting User PATH entry'
    $preownedEntries = Get-TestPathEntries -Configuration $preowned
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $preownedEntries -Path $preowned.BinDirectory) -Message 'preexisting User PATH entry was not deduplicated'
    Assert-Succeeded -Result (Invoke-TestUninstall -Configuration $preowned) -Label 'uninstall with a preexisting User PATH entry'
    $preownedEntriesAfterUninstall = Get-TestPathEntries -Configuration $preowned
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $preownedEntriesAfterUninstall -Path $preowned.BinDirectory) -Message 'uninstall removed a User PATH entry it did not own'
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $preownedEntriesAfterUninstall -Path $preownedUnrelated) -Message 'uninstall changed an unrelated User PATH entry'

    $collision = New-TestPrefix -Name 'launcher collision'
    [System.IO.Directory]::CreateDirectory($collision.BinDirectory) | Out-Null
    $collisionLauncher = Join-Path -Path $collision.BinDirectory -ChildPath 'tessivum.cmd'
    [System.IO.File]::WriteAllText($collisionLauncher, 'third-party launcher', $script:Utf8NoBom)
    $collisionResult = Invoke-TestInstall -Configuration $collision -FixtureArchive $oldArchive -Version $variants.Old
    Assert-Failed -Result $collisionResult -Label 'external launcher collision'
    Assert-Equal -Expected 'third-party launcher' -Actual ([System.IO.File]::ReadAllText($collisionLauncher, $script:Utf8NoBom)) -Message 'installer replaced an external launcher'
    Assert-NoPartialInstall -Configuration $collision -Version $variants.Old

    $upgrade = New-TestPrefix -Name 'upgrade and rollback'
    Assert-Succeeded -Result (Invoke-TestInstall -Configuration $upgrade -FixtureArchive $oldArchive -Version $variants.Old) -Label 'initial versioned install'
    Assert-StableLaunchers -Configuration $upgrade -ExpectedVersion $variants.Old -Label 'initial versioned install'
    $upgradeDataDirectory = Join-Path -Path $upgrade.Home -ChildPath '.tessivum'
    [System.IO.Directory]::CreateDirectory($upgradeDataDirectory) | Out-Null
    $upgradeData = Join-Path -Path $upgradeDataDirectory -ChildPath 'state.marker'
    [System.IO.File]::WriteAllText($upgradeData, 'durable rollback state', $script:Utf8NoBom)
    $badChecksumArchive = Join-Path -Path $fixtureDirectory -ChildPath 'bad-checksum.zip'
    [System.IO.File]::Copy($newArchive, $badChecksumArchive, $true)
    $badChecksumName = 'tessivum-' + $variants.New + '-' + $script:Target + '.zip'
    [System.IO.File]::WriteAllText($badChecksumArchive + '.sha256', ((('0' * 64) -join '') + '  ' + $badChecksumName + "`r`n"), $script:Utf8NoBom)
    $badUpgrade = Invoke-TestInstall -Configuration $upgrade -FixtureArchive $badChecksumArchive -Version $variants.New
    Assert-Failed -Result $badUpgrade -Label 'checksum-failed upgrade'
    Assert-StableLaunchers -Configuration $upgrade -ExpectedVersion $variants.Old -Label 'checksum-failed upgrade rollback'
    Assert-NoPartialInstall -Configuration $upgrade -Version $variants.New
    Assert-Equal -Expected 'durable rollback state' -Actual ([System.IO.File]::ReadAllText($upgradeData, $script:Utf8NoBom)) -Message 'checksum-failed upgrade changed user data'

    $lockedAlias = Join-Path -Path $upgrade.BinDirectory -ChildPath 'tsv.cmd'
    $aliasLock = [System.IO.File]::Open($lockedAlias, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
    try {
        $partialUpgrade = Invoke-TestInstall -Configuration $upgrade -FixtureArchive $newArchive -Version $variants.New
    }
    finally {
        $aliasLock.Dispose()
    }
    Assert-Failed -Result $partialUpgrade -Label 'partially blocked launcher update'
    Assert-StableLaunchers -Configuration $upgrade -ExpectedVersion $variants.Old -Label 'partial launcher update rollback'
    Assert-NoPartialInstall -Configuration $upgrade -Version $variants.New
    Assert-Equal -Expected 'durable rollback state' -Actual ([System.IO.File]::ReadAllText($upgradeData, $script:Utf8NoBom)) -Message 'partial launcher update changed user data'
    $partialUpgradeEntries = Get-TestPathEntries -Configuration $upgrade
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $partialUpgradeEntries -Path $upgrade.BinDirectory) -Message 'partial launcher update rollback changed the owned User PATH entry'

    Assert-Succeeded -Result (Invoke-TestInstall -Configuration $upgrade -FixtureArchive $newArchive -Version $variants.New) -Label 'upgrade with real packaged executable'
    Assert-StableLaunchers -Configuration $upgrade -ExpectedVersion $variants.New -Label 'upgrade with real packaged executable'
    Assert-Succeeded -Result (Invoke-TestInstall -Configuration $upgrade -FixtureArchive $oldArchive -Version $variants.Old) -Label 'downgrade with real packaged executable'
    Assert-StableLaunchers -Configuration $upgrade -ExpectedVersion $variants.Old -Label 'downgrade with real packaged executable'
    Assert-Equal -Expected 'durable rollback state' -Actual ([System.IO.File]::ReadAllText($upgradeData, $script:Utf8NoBom)) -Message 'upgrade or downgrade changed user data'
    $wrongChecksumNameArchive = Join-Path -Path $fixtureDirectory -ChildPath 'wrong-checksum-basename.zip'
    [System.IO.File]::Copy($newArchive, $wrongChecksumNameArchive, $true)
    $newArchiveHash = (Get-FileHash -LiteralPath $wrongChecksumNameArchive -Algorithm SHA256).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText($wrongChecksumNameArchive + '.sha256', ($newArchiveHash + '  unrelated.zip' + "`r`n"), $script:Utf8NoBom)
    $wrongChecksumName = New-TestPrefix -Name 'wrong checksum basename'
    Assert-Failed -Result (Invoke-TestInstall -Configuration $wrongChecksumName -FixtureArchive $wrongChecksumNameArchive -Version $variants.New) -Label 'checksum with a wrong basename'
    Assert-NoPartialInstall -Configuration $wrongChecksumName -Version $variants.New

    $expectedArchiveName = 'tessivum-' + $release.Version + '-' + $script:Target + '.zip'
    $expectedRoot = [System.IO.Path]::GetFileNameWithoutExtension($expectedArchiveName)
    $wrongRootArchive = Join-Path -Path $fixtureDirectory -ChildPath 'wrong-root.zip'
    New-MinimalZipArchive -Path $wrongRootArchive -ExpectedArchiveName $expectedArchiveName -Entries @(
        [PSCustomObject]@{ Name = 'tessivum-wrong-root/bin/tessivum.cmd'; Content = [byte[]]@(); ExternalAttributes = $null }
    )
    $wrongRoot = New-TestPrefix -Name 'wrong root'
    Assert-Failed -Result (Invoke-TestInstall -Configuration $wrongRoot -FixtureArchive $wrongRootArchive -Version $release.Version) -Label 'wrong archive root'
    Assert-NoPartialInstall -Configuration $wrongRoot -Version $release.Version

    $zipSlipArchive = Join-Path -Path $fixtureDirectory -ChildPath 'zip-slip.zip'
    New-MinimalZipArchive -Path $zipSlipArchive -ExpectedArchiveName $expectedArchiveName -Entries @(
        [PSCustomObject]@{ Name = $expectedRoot + '/../../zip-slip-created.txt'; Content = [byte[]]@(0); ExternalAttributes = $null }
    )
    $zipSlip = New-TestPrefix -Name 'zip slip'
    $escapedPath = Join-Path -Path $zipSlip.InstallRoot -ChildPath 'zip-slip-created.txt'
    Assert-Failed -Result (Invoke-TestInstall -Configuration $zipSlip -FixtureArchive $zipSlipArchive -Version $release.Version) -Label 'ZIP Slip archive'
    Assert-NoPartialInstall -Configuration $zipSlip -Version $release.Version -EscapedPath $escapedPath

    $missingBinaryArchive = Join-Path -Path $fixtureDirectory -ChildPath 'missing-binary.zip'
    New-MinimalZipArchive -Path $missingBinaryArchive -ExpectedArchiveName $expectedArchiveName -Entries @(
        [PSCustomObject]@{ Name = $expectedRoot + '/bin/tessivum.cmd'; Content = 'incomplete release fixture'; ExternalAttributes = $null },
        [PSCustomObject]@{ Name = $expectedRoot + '/bin/tsv.cmd'; Content = 'incomplete release fixture'; ExternalAttributes = $null }
    )
    $missingBinary = New-TestPrefix -Name 'missing binary'
    Assert-Failed -Result (Invoke-TestInstall -Configuration $missingBinary -FixtureArchive $missingBinaryArchive -Version $release.Version) -Label 'archive without tessivum.exe'
    Assert-NoPartialInstall -Configuration $missingBinary -Version $release.Version

    $caseCollisionArchive = Join-Path -Path $fixtureDirectory -ChildPath 'case-collision.zip'
    New-MinimalZipArchive -Path $caseCollisionArchive -ExpectedArchiveName $expectedArchiveName -Entries @(
        [PSCustomObject]@{ Name = $expectedRoot + '/bin/tessivum.cmd'; Content = ''; ExternalAttributes = $null },
        [PSCustomObject]@{ Name = $expectedRoot + '/Bin/tsv.cmd'; Content = ''; ExternalAttributes = $null }
    )
    $caseCollision = New-TestPrefix -Name 'case collision'
    Assert-Failed -Result (Invoke-TestInstall -Configuration $caseCollision -FixtureArchive $caseCollisionArchive -Version $release.Version) -Label 'case-colliding archive'
    Assert-NoPartialInstall -Configuration $caseCollision -Version $release.Version

    $reparseArchive = Join-Path -Path $fixtureDirectory -ChildPath 'reparse-entry.zip'
    New-MinimalZipArchive -Path $reparseArchive -ExpectedArchiveName $expectedArchiveName -Entries @(
        [PSCustomObject]@{ Name = $expectedRoot + '/reparse-entry'; Content = [byte[]]@(0, 128, 255); ExternalAttributes = 0x0400 }
    )
    $reparse = New-TestPrefix -Name 'reparse entry'
    Assert-Failed -Result (Invoke-TestInstall -Configuration $reparse -FixtureArchive $reparseArchive -Version $release.Version) -Label 'reparse-point archive entry'
    Assert-NoPartialInstall -Configuration $reparse -Version $release.Version
    $symlinkArchive = Join-Path -Path $fixtureDirectory -ChildPath 'symlink-entry.zip'
    $symlinkAttributes = [System.BitConverter]::ToInt32([byte[]]@(0, 0, 0, 160), 0)
    New-MinimalZipArchive -Path $symlinkArchive -ExpectedArchiveName $expectedArchiveName -Entries @(
        [PSCustomObject]@{ Name = $expectedRoot + '/symlink-entry'; Content = 'unsafe'; ExternalAttributes = $symlinkAttributes }
    )
    $symlink = New-TestPrefix -Name 'symlink entry'
    Assert-Failed -Result (Invoke-TestInstall -Configuration $symlink -FixtureArchive $symlinkArchive -Version $release.Version) -Label 'ZIP symlink entry'
    Assert-NoPartialInstall -Configuration $symlink -Version $release.Version

    $uninstallRollback = New-TestPrefix -Name 'uninstall rollback retention'
    Assert-Succeeded -Result (Invoke-TestInstall -Configuration $uninstallRollback -FixtureArchive $archive -Version $release.Version) -Label 'uninstall rollback install of the real packaged executable'
    Assert-StableLaunchers -Configuration $uninstallRollback -ExpectedVersion $release.Version -Label 'uninstall rollback install'
    $rollbackVersionPath = Join-Path -Path $uninstallRollback.InstallRoot -ChildPath $release.Version
    $rollbackOwnershipPath = Join-Path -Path (Split-Path -Parent $uninstallRollback.InstallRoot) -ChildPath '.tessivum-path-owner'
    Assert-True -Condition ([System.IO.File]::Exists($rollbackOwnershipPath)) -Message 'uninstall rollback install did not create a PATH ownership record'
    $rollbackOwnershipAttributes = [System.IO.File]::GetAttributes($rollbackOwnershipPath)
    $rollbackOwnershipReadOnly = $false
    try {
        [System.IO.File]::SetAttributes(
            $rollbackOwnershipPath,
            ($rollbackOwnershipAttributes -bor [System.IO.FileAttributes]::ReadOnly)
        )
        $rollbackOwnershipReadOnly = $true
        $ordinaryUninstallFailure = Invoke-TestUninstall -Configuration $uninstallRollback
        Assert-Failed -Result $ordinaryUninstallFailure -Label 'read-only ownership-record uninstall rollback'
        Assert-True -Condition (-not (Get-ResultText -Result $ordinaryUninstallFailure).Contains('rollback failed:')) -Message 'read-only ownership-record uninstall did not complete rollback'
    }
    finally {
        if ($rollbackOwnershipReadOnly -and [System.IO.File]::Exists($rollbackOwnershipPath)) {
            [System.IO.File]::SetAttributes($rollbackOwnershipPath, $rollbackOwnershipAttributes)
        }
    }
    Assert-StableLaunchers -Configuration $uninstallRollback -ExpectedVersion $release.Version -Label 'read-only ownership-record uninstall rollback'
    Assert-True -Condition ([System.IO.Directory]::Exists($rollbackVersionPath)) -Message 'read-only ownership-record uninstall rollback did not restore the managed version'
    $rollbackEntries = Get-TestPathEntries -Configuration $uninstallRollback
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $rollbackEntries -Path $uninstallRollback.BinDirectory) -Message 'read-only ownership-record uninstall rollback did not restore the owned User PATH entry'

    $originalInstallRootAcl = Get-Acl -LiteralPath $uninstallRollback.InstallRoot
    $installRootAclChanged = $false
    $rollbackOwnershipReadOnly = $false
    $recoveryVersionStage = $null
    $recoveryVersionPath = $null
    try {
        [System.IO.File]::SetAttributes(
            $rollbackOwnershipPath,
            ($rollbackOwnershipAttributes -bor [System.IO.FileAttributes]::ReadOnly)
        )
        $rollbackOwnershipReadOnly = $true
        $updatedInstallRootAcl = Get-Acl -LiteralPath $uninstallRollback.InstallRoot
        $denyCreateDirectories = [System.Security.AccessControl.FileSystemAccessRule]::new(
            [System.Security.Principal.WindowsIdentity]::GetCurrent().User,
            [System.Security.AccessControl.FileSystemRights]::CreateDirectories,
            [System.Security.AccessControl.InheritanceFlags]::None,
            [System.Security.AccessControl.PropagationFlags]::None,
            [System.Security.AccessControl.AccessControlType]::Deny
        )
        [void]$updatedInstallRootAcl.AddAccessRule($denyCreateDirectories)
        Set-Acl -LiteralPath $uninstallRollback.InstallRoot -AclObject $updatedInstallRootAcl
        $installRootAclChanged = $true
        $restorationFailure = Invoke-TestUninstall -Configuration $uninstallRollback
        Assert-Failed -Result $restorationFailure -Label 'uninstall rollback restoration failure'
        $restorationFailureOutput = Get-ResultText -Result $restorationFailure
        Assert-True -Condition $restorationFailureOutput.Contains('rollback failed:') -Message 'uninstall restoration failure did not report failed rollback'
        $restorationFailureEntries = Get-TestPathEntries -Configuration $uninstallRollback
        Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $restorationFailureEntries -Path $uninstallRollback.BinDirectory) -Message 'uninstall restoration failure did not restore the owned User PATH entry'
        Assert-True -Condition ([System.IO.File]::Exists($rollbackOwnershipPath)) -Message 'uninstall restoration failure removed its PATH ownership record'
        $recoveryVersionStage = Get-PrivateStagingDirectory -Parent (Split-Path -Parent $uninstallRollback.InstallRoot) -Prefix '.tessivum-uninstall-versions' -Label 'uninstall restoration failure'
        $recoveryVersionPath = Join-Path -Path $recoveryVersionStage -ChildPath $release.Version
        Assert-True -Condition ([System.IO.Directory]::Exists($recoveryVersionPath)) -Message 'uninstall restoration failure discarded the staged managed version'
        Assert-True -Condition (-not [System.IO.Directory]::Exists($rollbackVersionPath)) -Message 'uninstall restoration failure did not exercise version restoration'
    }
    finally {
        try {
            if ($installRootAclChanged) {
                Set-Acl -LiteralPath $uninstallRollback.InstallRoot -AclObject $originalInstallRootAcl
            }
        }
        finally {
            if ($rollbackOwnershipReadOnly -and [System.IO.File]::Exists($rollbackOwnershipPath)) {
                [System.IO.File]::SetAttributes($rollbackOwnershipPath, $rollbackOwnershipAttributes)
            }
        }
    }
    [System.IO.Directory]::Move($recoveryVersionPath, $rollbackVersionPath)
    [System.IO.Directory]::Delete($recoveryVersionStage, $true)
    Assert-StableLaunchers -Configuration $uninstallRollback -ExpectedVersion $release.Version -Label 'recovery-material restoration of the real packaged executable'
    $recoveredEntries = Get-TestPathEntries -Configuration $uninstallRollback
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $recoveredEntries -Path $uninstallRollback.BinDirectory) -Message 'recovery-material restoration changed the owned User PATH entry'
    Assert-Succeeded -Result (Invoke-TestUninstall -Configuration $uninstallRollback) -Label 'uninstall after recovery-material restoration'

    $postCommitCleanup = New-TestPrefix -Name 'read-only uninstall cleanup'
    Assert-Succeeded -Result (Invoke-TestInstall -Configuration $postCommitCleanup -FixtureArchive $archive -Version $release.Version) -Label 'read-only cleanup install of the real packaged executable'
    Assert-StableLaunchers -Configuration $postCommitCleanup -ExpectedVersion $release.Version -Label 'read-only cleanup install'
    $postCommitBinary = Join-Path -Path (Join-Path -Path $postCommitCleanup.InstallRoot -ChildPath $release.Version) -ChildPath 'libexec\tessivum.exe'
    Assert-True -Condition ([System.IO.File]::Exists($postCommitBinary)) -Message 'read-only cleanup install is missing the real packaged executable'
    $postCommitReadOnly = $false
    try {
        [System.IO.File]::SetAttributes(
            $postCommitBinary,
            ([System.IO.File]::GetAttributes($postCommitBinary) -bor [System.IO.FileAttributes]::ReadOnly)
        )
        $postCommitReadOnly = $true
        $postCommitFailure = Invoke-TestUninstall -Configuration $postCommitCleanup
        Assert-Failed -Result $postCommitFailure -Label 'read-only managed-file uninstall cleanup'
        $postCommitFailureOutput = Get-ResultText -Result $postCommitFailure
        Assert-True -Condition $postCommitFailureOutput.Contains('uninstall committed, but cleanup failed:') -Message 'read-only managed-file cleanup did not report a committed uninstall'
        Assert-True -Condition (-not $postCommitFailureOutput.Contains('rollback failed:')) -Message 'read-only managed-file cleanup attempted rollback after commit'
        Assert-True -Condition (-not [System.IO.File]::Exists((Join-Path -Path $postCommitCleanup.BinDirectory -ChildPath 'tessivum.cmd'))) -Message 'read-only managed-file cleanup restored the canonical launcher after commit'
        Assert-True -Condition (-not [System.IO.File]::Exists((Join-Path -Path $postCommitCleanup.BinDirectory -ChildPath 'tsv.cmd'))) -Message 'read-only managed-file cleanup restored the alias launcher after commit'
        Assert-True -Condition (-not [System.IO.Directory]::Exists((Join-Path -Path $postCommitCleanup.InstallRoot -ChildPath $release.Version))) -Message 'read-only managed-file cleanup restored the managed version after commit'
        $postCommitEntries = Get-TestPathEntries -Configuration $postCommitCleanup
        Assert-Equal -Expected 0 -Actual (Get-PathEntryCount -Entries $postCommitEntries -Path $postCommitCleanup.BinDirectory) -Message 'read-only managed-file cleanup restored the owned User PATH entry after commit'
        $postCommitVersionStage = Get-PrivateStagingDirectory -Parent (Split-Path -Parent $postCommitCleanup.InstallRoot) -Prefix '.tessivum-uninstall-versions' -Label 'read-only managed-file cleanup'
        $stagedPostCommitBinary = Join-Path -Path (Join-Path -Path $postCommitVersionStage -ChildPath $release.Version) -ChildPath 'libexec\tessivum.exe'
        Assert-True -Condition ([System.IO.File]::Exists($stagedPostCommitBinary)) -Message 'read-only managed-file cleanup discarded the remaining cleanup material'
        Assert-Succeeded -Result (Invoke-TestUninstall -Configuration $postCommitCleanup) -Label 'idempotent uninstall after committed cleanup failure'
    }
    finally {
        if ($postCommitReadOnly) {
            Clear-ReadOnlyFiles -Root $postCommitCleanup.Base
        }
    }
    $externalAlias = Join-Path -Path $upgrade.BinDirectory -ChildPath 'tsv.cmd'
    [System.IO.File]::Delete($externalAlias)
    [System.IO.File]::WriteAllText($externalAlias, 'third-party alias', $script:Utf8NoBom)
    Assert-Succeeded -Result (Invoke-TestUninstall -Configuration $upgrade) -Label 'uninstall with an external alias launcher'
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path -Path $upgrade.BinDirectory -ChildPath 'tessivum.cmd'))) -Message 'uninstall left its managed canonical launcher beside an external alias'
    Assert-Equal -Expected 'third-party alias' -Actual ([System.IO.File]::ReadAllText($externalAlias, $script:Utf8NoBom)) -Message 'uninstall removed or changed an external alias launcher'
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path -Path $upgrade.InstallRoot -ChildPath $variants.Old))) -Message 'uninstall left a managed downgraded version directory'

    $durableDataDirectory = Join-Path -Path $actual.Home -ChildPath '.tessivum'
    [System.IO.Directory]::CreateDirectory($durableDataDirectory) | Out-Null
    $durableData = Join-Path -Path $durableDataDirectory -ChildPath 'state.marker'
    [System.IO.File]::WriteAllText($durableData, 'durable user state', $script:Utf8NoBom)
    Assert-Succeeded -Result (Invoke-TestUninstall -Configuration $actual) -Label 'uninstall of real packaged executable'
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path -Path $actual.InstallRoot -ChildPath $release.Version))) -Message 'uninstall left its managed version directory'
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path -Path $actual.BinDirectory -ChildPath 'tessivum.cmd'))) -Message 'uninstall left its managed tessivum launcher'
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path -Path $actual.BinDirectory -ChildPath 'tsv.cmd'))) -Message 'uninstall left its managed tsv launcher'
    Assert-Equal -Expected 'durable user state' -Actual ([System.IO.File]::ReadAllText($durableData, $script:Utf8NoBom)) -Message 'uninstall removed user data'
    $actualEntriesAfterUninstall = Get-TestPathEntries -Configuration $actual
    Assert-Equal -Expected 0 -Actual (Get-PathEntryCount -Entries $actualEntriesAfterUninstall -Path $actual.BinDirectory) -Message 'uninstall did not remove the User PATH entry it owned'
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $actualEntriesAfterUninstall -Path $unrelatedOne) -Message 'uninstall changed the first unrelated User PATH entry'
    Assert-Equal -Expected 1 -Actual (Get-PathEntryCount -Entries $actualEntriesAfterUninstall -Path $unrelatedTwo) -Message 'uninstall changed the second unrelated User PATH entry'
    $unknownInstallLeftover = Join-Path -Path $actual.InstallRoot -ChildPath 'unmanaged-leftover.txt'
    [System.IO.File]::WriteAllText($unknownInstallLeftover, 'keep this unknown file', $script:Utf8NoBom)
    $unknownLeftoverUninstall = Invoke-TestUninstall -Configuration $actual
    Assert-Failed -Result $unknownLeftoverUninstall -Label 'uninstall with unknown retained install-root content'
    Assert-Equal -Expected 'keep this unknown file' -Actual ([System.IO.File]::ReadAllText($unknownInstallLeftover, $script:Utf8NoBom)) -Message 'uninstall removed unknown retained install-root content'
    [System.IO.File]::Delete($unknownInstallLeftover)
    Assert-Succeeded -Result (Invoke-TestUninstall -Configuration $actual) -Label 'idempotent uninstall'

    Write-Output 'Windows installer behavioral tests passed'
}
catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    [Console]::Error.WriteLine($_.ScriptStackTrace)
    exit 1
}
finally {
    if (-not [string]::IsNullOrEmpty($script:WorkDirectory) -and [System.IO.Directory]::Exists($script:WorkDirectory)) {
        try {
            [System.IO.Directory]::Delete($script:WorkDirectory, $true)
        }
        catch {
        }
    }
}
