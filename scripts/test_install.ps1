[CmdletBinding(DefaultParameterSetName = 'Archive')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Archive')]
    [ValidateNotNullOrEmpty()]
    [string]$ArchivePath,
    [Parameter(Mandatory = $true, ParameterSetName = 'RegistryOnly')]
    [switch]$RegistryOnly,
    [Parameter(ParameterSetName = 'Archive')]
    [Parameter(ParameterSetName = 'RegistryOnly')]
    [ValidateNotNullOrEmpty()]
    [string]$InstallerPath,
    [Parameter(ParameterSetName = 'Archive')]
    [switch]$RegistryPathRegression,
    [int]$ExpectedPowerShellMajor = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$script:Target = 'x86_64-pc-windows-msvc'
$script:WorkDirectory = $null
$script:TestScriptPath = $PSCommandPath
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
        RegistryOverrideRootSubKey = $null
        RegistryOverrideProbe = $null
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

function Get-RegistryOverrideBootstrapContent {
    return @'
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$InstallerPath,
    [Parameter(Mandatory = $true)][string]$RegistryOverrideRootSubKey,
    [Parameter(Mandatory = $true)][string]$RegistryOverrideProbe,
    [Parameter(Mandatory = $true)][string]$InstallerArgumentsBase64,
    [switch]$LoadPathStoreFunctions,
    [switch]$FailNotifierAddType
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($null -eq ('TessivumInstallerTest.RegistryOverrideNativeMethods' -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

namespace TessivumInstallerTest
{
    public static class RegistryOverrideNativeMethods
    {
        [DllImport("advapi32.dll", SetLastError = true)]
        public static extern int RegOverridePredefKey(IntPtr hKey, IntPtr hNewHKey);

        public static int OverrideCurrentUser(IntPtr replacement)
        {
            return RegOverridePredefKey(new IntPtr(unchecked((int)0x80000001)), replacement);
        }

        public static int RestoreCurrentUser()
        {
            return RegOverridePredefKey(new IntPtr(unchecked((int)0x80000001)), IntPtr.Zero);
        }
    }
}
"@
}

$rootKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($RegistryOverrideRootSubKey, $true)
if ($null -eq $rootKey) {
    throw "temporary registry root does not exist: $RegistryOverrideRootSubKey"
}

$overrideActive = $false
$environmentKey = $null
$exitCode = 0
try {
    $status = [TessivumInstallerTest.RegistryOverrideNativeMethods]::OverrideCurrentUser($rootKey.Handle.DangerousGetHandle())
    if ($status -ne 0) {
        throw "RegOverridePredefKey failed: $status"
    }
    $overrideActive = $true

    $environmentKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $false)
    if ($null -eq $environmentKey) {
        throw 'temporary HKCU override has no Environment key'
    }
    try {
        $missing = [System.Object]::new()
        $probe = $environmentKey.GetValue(
            'TessivumRegistryOverrideProbe',
            $missing,
            [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
        )
        if ([System.Object]::ReferenceEquals($probe, $missing) -or -not [string]::Equals([string]$probe, $RegistryOverrideProbe, [System.StringComparison]::Ordinal)) {
            throw 'temporary HKCU override probe did not resolve through the redirected Environment key'
        }
        $environmentApiProbe = [Environment]::GetEnvironmentVariable('TessivumRegistryOverrideProbe', [EnvironmentVariableTarget]::User)
        if (-not [string]::Equals($environmentApiProbe, $RegistryOverrideProbe, [System.StringComparison]::Ordinal)) {
            throw 'Environment.GetEnvironmentVariable did not resolve through the redirected Environment key'
        }
    }
    finally {
        $environmentKey.Close()
        $environmentKey = $null
    }

    if ($LoadPathStoreFunctions.IsPresent) {
        $tokens = $null
        $parseErrors = $null
        $installerAst = [System.Management.Automation.Language.Parser]::ParseFile($InstallerPath, [ref]$tokens, [ref]$parseErrors)
        if ($null -ne $parseErrors -and $parseErrors.Count -gt 0) {
            throw 'cannot parse installer functions for the registry-only regression'
        }

        $functionNames = @(
            'Fail',
            'Get-UserPathRegistryKey',
            'Get-PathStoreSnapshot',
            'Get-PathStoreWriteKind',
            'Send-EnvironmentChangedBroadcast',
            'Set-PathStoreValue',
            'Restore-PathStoreSnapshot',
            'Get-InstalledPathUpdate'
        )
        $definitionText = [System.Text.StringBuilder]::new()
        foreach ($functionName in $functionNames) {
            $predicate = {
                param($node)

                return $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and [string]::Equals($node.Name, $functionName, [System.StringComparison]::Ordinal)
            }.GetNewClosure()
            $definitions = @($installerAst.FindAll($predicate, $true))
            if ($definitions.Count -ne 1) {
                throw "installer regression function was not found exactly once: $functionName"
            }
            [void]$definitionText.AppendLine($definitions[0].Extent.Text)
        }

        $script:Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
        . ([scriptblock]::Create($definitionText.ToString()))

        $snapshot = Get-PathStoreSnapshot -PathStore $null
        if ($snapshot.Kind -ne 'registry') {
            Fail 'registry-only regression did not load the registry PATH store'
        }
        $probeEntry = 'C:\TessivumRegistryProbe\' + [System.Guid]::NewGuid().ToString('N')
        $update = Get-InstalledPathUpdate -CurrentValue $snapshot.Value -BinDirectory $probeEntry
        if (-not $update.Changed) {
            Fail 'registry-only regression did not create a distinct PATH update'
        }

        $written = $false
        try {
            $written = $true
            Set-PathStoreValue `
                -PathStore $null `
                -Value $update.Value `
                -RegistryValueKind (Get-PathStoreWriteKind -Snapshot $snapshot)

            $updatedSnapshot = Get-PathStoreSnapshot -PathStore $null
            if (-not $updatedSnapshot.Exists -or -not [string]::Equals($updatedSnapshot.Value, $update.Value, [System.StringComparison]::Ordinal)) {
                Fail 'registry-only regression did not retain the raw updated PATH value'
            }
            if ($updatedSnapshot.RegistryValueKind -ne (Get-PathStoreWriteKind -Snapshot $snapshot)) {
                Fail 'registry-only regression changed the PATH registry value kind'
            }
        }
        finally {
            if ($written) {
                Restore-PathStoreSnapshot -Snapshot $snapshot -PathStore $null
            }
        }

        $restoredSnapshot = Get-PathStoreSnapshot -PathStore $null
        if (([bool]$restoredSnapshot.Exists) -ne ([bool]$snapshot.Exists)) {
            Fail 'registry-only regression did not restore PATH value existence'
        }
        if ($snapshot.Exists) {
            if (-not [string]::Equals($restoredSnapshot.Value, $snapshot.Value, [System.StringComparison]::Ordinal)) {
                Fail 'registry-only regression did not restore the raw PATH value'
            }
            if ($restoredSnapshot.RegistryValueKind -ne $snapshot.RegistryValueKind) {
                Fail 'registry-only regression did not restore the PATH registry value kind'
            }
        }
    }
    else {
        $installerArgumentsJson = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($InstallerArgumentsBase64))
        $installerPayload = ConvertFrom-Json -InputObject $installerArgumentsJson
        $installerParameters = @{}
        foreach ($argument in @($installerPayload.Arguments)) {
            if ($argument -eq '-Uninstall') {
                $installerParameters.Uninstall = $true
            }
            else {
                $installerParameters.Version = [string]$argument
            }
        }
        if ($FailNotifierAddType.IsPresent) {
            $global:TessivumInstallerTestFailNotifierAddType = $true
            function global:Add-Type {
                [CmdletBinding(DefaultParameterSetName = 'AssemblyName')]
                param(
                    [Parameter(Mandatory = $true, ParameterSetName = 'AssemblyName')]
                    [string[]]$AssemblyName,
                    [Parameter(Mandatory = $true, ParameterSetName = 'TypeDefinition')]
                    [string]$TypeDefinition
                )

                if (
                    $PSCmdlet.ParameterSetName -eq 'TypeDefinition' -and
                    $global:TessivumInstallerTestFailNotifierAddType -and
                    $TypeDefinition.Contains('namespace TessivumInstaller') -and
                    $TypeDefinition.Contains('EnvironmentChangeNotifier')
                ) {
                    $global:TessivumInstallerTestFailNotifierAddType = $false
                    throw 'injected notifier Add-Type initialization failure'
                }
                if ($PSCmdlet.ParameterSetName -eq 'AssemblyName') {
                    Microsoft.PowerShell.Utility\Add-Type -AssemblyName $AssemblyName
                    return
                }
                Microsoft.PowerShell.Utility\Add-Type -TypeDefinition $TypeDefinition
            }
        }

        $global:LASTEXITCODE = 0
        & $InstallerPath @installerParameters
        if ($LASTEXITCODE -is [int]) {
            $exitCode = $LASTEXITCODE
        }
    }
}
finally {
    if ($null -ne $environmentKey) {
        $environmentKey.Close()
    }
    if ($overrideActive) {
        $restoreStatus = [TessivumInstallerTest.RegistryOverrideNativeMethods]::RestoreCurrentUser()
        if ($restoreStatus -ne 0) {
            throw "RegOverridePredefKey restoration failed: $restoreStatus"
        }
    }
    $rootKey.Close()
}

exit $exitCode
'@
}

function Invoke-InstallerProcess {
    param(
        [Parameter(Mandatory = $true)][hashtable]$Overrides,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$InstallerArguments,
        [AllowNull()][string]$RegistryOverrideRootSubKey,
        [AllowNull()][string]$RegistryOverrideProbe,
        [switch]$LoadPathStoreFunctions,
        [switch]$FailNotifierAddType
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

        if ([string]::IsNullOrEmpty($RegistryOverrideRootSubKey)) {
            Assert-True -Condition (-not $LoadPathStoreFunctions.IsPresent) -Message 'registry AST loading requires an isolated HKCU override'
            $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $script:InstallerPath)
            $arguments += $InstallerArguments
        }
        else {
            Assert-True -Condition (-not [string]::IsNullOrEmpty($RegistryOverrideProbe)) -Message 'registry override requires a probe value'
            $bootstrapPath = Join-Path -Path $script:WorkDirectory -ChildPath ('registry-override-' + [System.Guid]::NewGuid().ToString('N') + '.ps1')
            [System.IO.File]::WriteAllText($bootstrapPath, (Get-RegistryOverrideBootstrapContent), $script:Utf8NoBom)
            $payload = [PSCustomObject]@{ Arguments = @($InstallerArguments) }
            $argumentsJson = ConvertTo-Json -InputObject $payload -Compress
            $argumentsBase64 = [System.Convert]::ToBase64String($script:Utf8NoBom.GetBytes($argumentsJson))
            $arguments = @(
                '-NoProfile',
                '-ExecutionPolicy',
                'Bypass',
                '-File',
                $bootstrapPath,
                '-InstallerPath',
                $script:InstallerPath,
                '-RegistryOverrideRootSubKey',
                $RegistryOverrideRootSubKey,
                '-RegistryOverrideProbe',
                $RegistryOverrideProbe,
                '-InstallerArgumentsBase64',
                $argumentsBase64
            )
            if ($LoadPathStoreFunctions.IsPresent) {
                $arguments += '-LoadPathStoreFunctions'
            }
            if ($FailNotifierAddType.IsPresent) {
                $arguments += '-FailNotifierAddType'
            }
        }

        $previousErrorActionPreference = $ErrorActionPreference
        try {
            # PowerShell 5.1 surfaces redirected native stderr as error records.
            # Capture those records and assert the actual exit code in the caller.
            $ErrorActionPreference = 'Continue'
            $global:LASTEXITCODE = 1
            $output = & $script:PowerShellHost @arguments 2>&1
            $exitCode = $LASTEXITCODE
        }
        finally {
            $ErrorActionPreference = $previousErrorActionPreference
        }
        return [PSCustomObject]@{
            ExitCode = $exitCode
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
        [Parameter(Mandatory = $true)][string]$Version,
        [switch]$FailNotifierAddType
    )

    $overrides = @{
        TESSIVUM_INSTALLER_TEST = '1'
        FIXTURE_URL = Get-FileUri -Path $FixtureArchive
        INSTALL_ROOT = $Configuration.InstallRoot
        BIN_DIR = $Configuration.BinDirectory
        USERPROFILE = $Configuration.Home
        LOCALAPPDATA = $Configuration.LocalAppData
    }
    if (-not [string]::IsNullOrEmpty($Configuration.PathStore)) {
        $overrides.PATH_STORE = $Configuration.PathStore
    }
    return Invoke-InstallerProcess `
        -Overrides $overrides `
        -InstallerArguments @($Version) `
        -RegistryOverrideRootSubKey $Configuration.RegistryOverrideRootSubKey `
        -RegistryOverrideProbe $Configuration.RegistryOverrideProbe `
        -FailNotifierAddType:$FailNotifierAddType
}


function Invoke-TestUninstall {
    param(
        [Parameter(Mandatory = $true)]$Configuration,
        [switch]$FailNotifierAddType
    )

    $overrides = @{
        TESSIVUM_INSTALLER_TEST = '1'
        INSTALL_ROOT = $Configuration.InstallRoot
        BIN_DIR = $Configuration.BinDirectory
        USERPROFILE = $Configuration.Home
        LOCALAPPDATA = $Configuration.LocalAppData
    }
    if (-not [string]::IsNullOrEmpty($Configuration.PathStore)) {
        $overrides.PATH_STORE = $Configuration.PathStore
    }
    return Invoke-InstallerProcess `
        -Overrides $overrides `
        -InstallerArguments @('-Uninstall') `
        -RegistryOverrideRootSubKey $Configuration.RegistryOverrideRootSubKey `
        -RegistryOverrideProbe $Configuration.RegistryOverrideProbe `
        -FailNotifierAddType:$FailNotifierAddType
}

function Invoke-TestPathStoreRegression {
    param([Parameter(Mandatory = $true)]$Configuration)

    $overrides = @{
        TESSIVUM_INSTALLER_TEST = '1'
    }
    return Invoke-InstallerProcess `
        -Overrides $overrides `
        -InstallerArguments @() `
        -RegistryOverrideRootSubKey $Configuration.RegistryOverrideRootSubKey `
        -RegistryOverrideProbe $Configuration.RegistryOverrideProbe `
        -LoadPathStoreFunctions
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
        $global:LASTEXITCODE = 1
        $output = & $LauncherPath '--version' 2>&1
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

function Wait-ForExclusiveFileAccess {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label,
        [ValidateRange(1, 30000)][int]$TimeoutMilliseconds = 5000
    )

    $deadline = [System.DateTime]::UtcNow.AddMilliseconds($TimeoutMilliseconds)
    $lastFailure = $null
    $successfulPolls = 0
    while ($true) {
        $stream = $null
        try {
            $stream = [System.IO.File]::Open(
                $Path,
                [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::ReadWrite,
                [System.IO.FileShare]::None
            )
            $successfulPolls++
        }
        catch [System.IO.IOException] {
            $successfulPolls = 0
            $lastFailure = $_
        }
        catch [System.UnauthorizedAccessException] {
            $successfulPolls = 0
            $lastFailure = $_
        }
        finally {
            if ($null -ne $stream) {
                $stream.Dispose()
            }
        }

        if ($successfulPolls -ge 5) {
            return
        }

        if ([System.DateTime]::UtcNow -ge $deadline) {
            Fail "$Label did not release ${Path}: $($lastFailure.Exception.Message)"
        }
        Start-Sleep -Milliseconds 50
    }
}

function Wait-ForDirectoryMove {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label,
        [ValidateRange(1, 30000)][int]$TimeoutMilliseconds = 5000
    )

    $probePath = Join-Path -Path (Split-Path -Parent $Path) -ChildPath ('.tessivum-move-probe-' + [System.Guid]::NewGuid().ToString('N'))
    $deadline = [System.DateTime]::UtcNow.AddMilliseconds($TimeoutMilliseconds)
    $lastFailure = $null
    $successfulPolls = 0
    $moved = $false
    try {
        while ($true) {
            try {
                [System.IO.Directory]::Move($Path, $probePath)
                $moved = $true
                [System.IO.Directory]::Move($probePath, $Path)
                $moved = $false
                $successfulPolls++
            }
            catch [System.IO.IOException] {
                $successfulPolls = 0
                $lastFailure = $_
            }
            catch [System.UnauthorizedAccessException] {
                $successfulPolls = 0
                $lastFailure = $_
            }

            if ($successfulPolls -ge 5) {
                return
            }

            if ($moved) {
                throw $lastFailure
            }
            if ([System.DateTime]::UtcNow -ge $deadline) {
                Fail "$Label did not release ${Path}: $($lastFailure.Exception.Message)"
            }
            Start-Sleep -Milliseconds 50
        }
    }
    finally {
        if ($moved -and [System.IO.Directory]::Exists($probePath) -and -not [System.IO.Directory]::Exists($Path)) {
            [System.IO.Directory]::Move($probePath, $Path)
        }
    }
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

function New-RegistryPathTestPrefix {
    param([Parameter(Mandatory = $true)][string]$Name)

    $configuration = New-TestPrefix -Name $Name
    $configuration.PathStore = $null
    $configuration.RegistryOverrideRootSubKey = 'Software\Tessivum\InstallerTests\' + [System.Guid]::NewGuid().ToString('N')
    $configuration.RegistryOverrideProbe = [System.Guid]::NewGuid().ToString('N')

    $rootKey = $null
    $environmentKey = $null
    try {
        $rootKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($configuration.RegistryOverrideRootSubKey)
        $environmentKey = $rootKey.CreateSubKey('Environment')
        $environmentKey.SetValue(
            'TessivumRegistryOverrideProbe',
            $configuration.RegistryOverrideProbe,
            [Microsoft.Win32.RegistryValueKind]::String
        )
    }
    finally {
        if ($null -ne $environmentKey) {
            $environmentKey.Close()
        }
        if ($null -ne $rootKey) {
            $rootKey.Close()
        }
    }

    return $configuration
}

function Remove-RegistryPathTestPrefix {
    param([Parameter(Mandatory = $true)]$Configuration)

    $prefix = 'Software\Tessivum\InstallerTests\'
    $rootSubKey = $Configuration.RegistryOverrideRootSubKey
    if ([string]::IsNullOrEmpty($rootSubKey) -or -not $rootSubKey.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        Fail 'registry test attempted to remove an invalid temporary key'
    }
    $leaf = $rootSubKey.Substring($prefix.Length)
    if ([string]::IsNullOrEmpty($leaf) -or $leaf.IndexOf('\', [System.StringComparison]::Ordinal) -ge 0) {
        Fail 'registry test attempted to remove an invalid temporary key leaf'
    }

    $parentKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\Tessivum\InstallerTests', $true)
    if ($null -eq $parentKey) {
        return
    }
    try {
        $childKey = $parentKey.OpenSubKey($leaf, $false)
        if ($null -ne $childKey) {
            $childKey.Close()
            $parentKey.DeleteSubKeyTree($leaf)
        }
    }
    finally {
        $parentKey.Close()
    }
}

function Set-TestRegistryPathValue {
    [CmdletBinding(DefaultParameterSetName = 'Write')]
    param(
        [Parameter(Mandatory = $true)]$Configuration,
        [Parameter(Mandatory = $true, ParameterSetName = 'Write')]
        [AllowEmptyString()][string]$Value,
        [Parameter(Mandatory = $true, ParameterSetName = 'Delete')]
        [switch]$Delete,
        [Microsoft.Win32.RegistryValueKind]$RegistryValueKind = [Microsoft.Win32.RegistryValueKind]::String
    )

    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($Configuration.RegistryOverrideRootSubKey + '\Environment', $true)
    if ($null -eq $key) {
        Fail 'temporary registry Environment key does not exist'
    }
    try {
        if ($Delete.IsPresent) {
            $key.DeleteValue('Path', $false)
        }
        else {
            $key.SetValue('Path', $Value, $RegistryValueKind)
        }
    }
    finally {
        $key.Close()
    }
}

function Get-TestRegistryPathState {
    param([Parameter(Mandatory = $true)]$Configuration)

    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($Configuration.RegistryOverrideRootSubKey + '\Environment', $false)
    if ($null -eq $key) {
        Fail 'temporary registry Environment key does not exist'
    }
    try {
        $missing = [System.Object]::new()
        $value = $key.GetValue(
            'Path',
            $missing,
            [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
        )
        if ([System.Object]::ReferenceEquals($value, $missing)) {
            return [PSCustomObject]@{
                Exists = $false
                Value = $null
                RegistryValueKind = $null
                ExpandedValue = $null
            }
        }
        return [PSCustomObject]@{
            Exists = $true
            Value = $value
            RegistryValueKind = $key.GetValueKind('Path')
            ExpandedValue = $key.GetValue('Path', $missing)
        }
    }
    finally {
        $key.Close()
    }
}

function Assert-TestRegistryPathState {
    param(
        [Parameter(Mandatory = $true)]$Configuration,
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][bool]$ExpectedExists,
        [AllowNull()][AllowEmptyString()][string]$ExpectedValue,
        [AllowNull()]$ExpectedRegistryValueKind,
        [AllowNull()][AllowEmptyString()][string]$ExpectedExpandedValue
    )

    $actual = Get-TestRegistryPathState -Configuration $Configuration
    Assert-Equal -Expected $ExpectedExists -Actual $actual.Exists -Message "$Label PATH existence"
    if (-not $ExpectedExists) {
        return
    }
    Assert-True -Condition ($actual.Value -is [string]) -Message "$Label PATH raw value is not a string"
    Assert-Equal -Expected $ExpectedValue -Actual $actual.Value -Message "$Label PATH raw value"
    Assert-Equal -Expected $ExpectedRegistryValueKind -Actual $actual.RegistryValueKind -Message "$Label PATH registry value kind"
    if ($PSBoundParameters.ContainsKey('ExpectedExpandedValue')) {
        Assert-Equal -Expected $ExpectedExpandedValue -Actual $actual.ExpandedValue -Message "$Label PATH expanded view"
    }
}

function Get-ExpectedInstalledPathValue {
    param(
        [AllowNull()]$CurrentValue,
        [Parameter(Mandatory = $true)][string]$BinDirectory
    )

    if ($null -eq $CurrentValue) {
        return $BinDirectory
    }
    return ($CurrentValue + ';' + $BinDirectory)
}

function Invoke-RegistryPathRoundTrip {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$FixtureArchive,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][bool]$InitialExists,
        [AllowNull()]$InitialValue,
        [Microsoft.Win32.RegistryValueKind]$InitialRegistryValueKind = [Microsoft.Win32.RegistryValueKind]::String,
        [Parameter(Mandatory = $true)][bool]$CheckExpandedValue,
        [AllowNull()]$ExpectedInitialExpandedValue,
        [Parameter(Mandatory = $true)][bool]$AssertRawAndExpandedDiffer,
        [Parameter(Mandatory = $true)][bool]$ExerciseUninstallRollback,
        [bool]$UseMissingInstallPathCleanup = $false
    )

    $configuration = New-RegistryPathTestPrefix -Name $Name
    try {
        if ($InitialExists) {
            Set-TestRegistryPathValue `
                -Configuration $configuration `
                -Value $InitialValue `
                -RegistryValueKind $InitialRegistryValueKind
        }
        else {
            Set-TestRegistryPathValue -Configuration $configuration -Delete
        }

        $initialStateArguments = @{
            Configuration = $configuration
            Label = "$Name initial"
            ExpectedExists = $InitialExists
            ExpectedValue = $InitialValue
            ExpectedRegistryValueKind = $InitialRegistryValueKind
        }
        if ($CheckExpandedValue) {
            $initialStateArguments.ExpectedExpandedValue = $ExpectedInitialExpandedValue
        }
        Assert-TestRegistryPathState @initialStateArguments
        if ($AssertRawAndExpandedDiffer) {
            $initialState = Get-TestRegistryPathState -Configuration $configuration
            Assert-True -Condition (-not [string]::Equals($initialState.Value, $initialState.ExpandedValue, [System.StringComparison]::Ordinal)) -Message "$Name initial PATH raw value unexpectedly equals its expanded view"
        }

        $installResult = Invoke-TestInstall -Configuration $configuration -FixtureArchive $FixtureArchive -Version $Version
        Assert-Succeeded -Result $installResult -Label "$Name install"
        $installedValue = Get-ExpectedInstalledPathValue -CurrentValue $InitialValue -BinDirectory $configuration.BinDirectory
        $installedRegistryValueKind = if ($InitialExists) {
            $InitialRegistryValueKind
        }
        else {
            [Microsoft.Win32.RegistryValueKind]::String
        }
        $installedStateArguments = @{
            Configuration = $configuration
            Label = "$Name install"
            ExpectedExists = $true
            ExpectedValue = $installedValue
            ExpectedRegistryValueKind = $installedRegistryValueKind
        }
        if ($CheckExpandedValue) {
            $installedStateArguments.ExpectedExpandedValue = (Get-ExpectedInstalledPathValue -CurrentValue $ExpectedInitialExpandedValue -BinDirectory $configuration.BinDirectory)
        }
        Assert-TestRegistryPathState @installedStateArguments
        if ($AssertRawAndExpandedDiffer) {
            $installedState = Get-TestRegistryPathState -Configuration $configuration
            Assert-True -Condition (-not [string]::Equals($installedState.Value, $installedState.ExpandedValue, [System.StringComparison]::Ordinal)) -Message "$Name installed PATH raw value unexpectedly equals its expanded view"
        }

        if ($ExerciseUninstallRollback) {
            $ownershipPath = Join-Path -Path (Split-Path -Parent $configuration.InstallRoot) -ChildPath '.tessivum-path-owner'
            Assert-True -Condition ([System.IO.File]::Exists($ownershipPath)) -Message "$Name install did not create a PATH ownership record"
            $ownershipAttributes = [System.IO.File]::GetAttributes($ownershipPath)
            $ownershipReadOnly = $false
            try {
                [System.IO.File]::SetAttributes(
                    $ownershipPath,
                    ($ownershipAttributes -bor [System.IO.FileAttributes]::ReadOnly)
                )
                $ownershipReadOnly = $true
                $rollbackResult = Invoke-TestUninstall -Configuration $configuration
                Assert-Failed -Result $rollbackResult -Label "$Name uninstall rollback"
                Assert-True -Condition (-not (Get-ResultText -Result $rollbackResult).Contains('rollback failed:')) -Message "$Name uninstall rollback did not complete"
                Assert-TestRegistryPathState @installedStateArguments
                if ($AssertRawAndExpandedDiffer) {
                    $rollbackState = Get-TestRegistryPathState -Configuration $configuration
                    Assert-True -Condition (-not [string]::Equals($rollbackState.Value, $rollbackState.ExpandedValue, [System.StringComparison]::Ordinal)) -Message "$Name rollback PATH raw value unexpectedly equals its expanded view"
                }
            }
            finally {
                if ($ownershipReadOnly -and [System.IO.File]::Exists($ownershipPath)) {
                    [System.IO.File]::SetAttributes($ownershipPath, $ownershipAttributes)
                }
            }
            Assert-StableLaunchers -Configuration $configuration -ExpectedVersion $Version -Label "$Name uninstall rollback"
            $rollbackVersionPath = Join-Path -Path $configuration.InstallRoot -ChildPath $Version
            Wait-ForExclusiveFileAccess `
                -Path (Join-Path -Path $rollbackVersionPath -ChildPath 'libexec\tessivum.exe') `
                -Label "$Name uninstall rollback launcher"
            Wait-ForDirectoryMove `
                -Path $rollbackVersionPath `
                -Label "$Name uninstall rollback version directory"
        }

        if ($UseMissingInstallPathCleanup) {
            [System.IO.File]::Delete((Join-Path -Path $configuration.BinDirectory -ChildPath 'tessivum.cmd'))
            [System.IO.File]::Delete((Join-Path -Path $configuration.BinDirectory -ChildPath 'tsv.cmd'))
            $retainedInstallRoot = $configuration.InstallRoot + '-retained'
            [System.IO.Directory]::Move($configuration.InstallRoot, $retainedInstallRoot)

            $ownershipPath = Join-Path -Path (Split-Path -Parent $configuration.InstallRoot) -ChildPath '.tessivum-path-owner'
            $rollbackResult = Invoke-TestUninstall -Configuration $configuration -FailNotifierAddType
            Assert-Failed -Result $rollbackResult -Label "$Name missing-install notifier rollback"
            $rollbackText = Get-ResultText -Result $rollbackResult
            Assert-True -Condition $rollbackText.Contains('injected notifier Add-Type initialization failure') -Message ("$Name missing-install notifier failure was suppressed: " + $rollbackText)
            Assert-True -Condition (-not $rollbackText.Contains('rollback failed:')) -Message "$Name missing-install notifier rollback did not complete"
            Assert-TestRegistryPathState @installedStateArguments
            Assert-True -Condition ([System.IO.File]::Exists($ownershipPath)) -Message "$Name missing-install notifier rollback removed the PATH ownership record"
        }

        $uninstallResult = Invoke-TestUninstall -Configuration $configuration
        Assert-Succeeded -Result $uninstallResult -Label "$Name uninstall"
        Assert-TestRegistryPathState @initialStateArguments
    }
    finally {
        Remove-RegistryPathTestPrefix -Configuration $configuration
    }
}

function Invoke-RegistryOnlyPathRoundTrip {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][bool]$InitialExists,
        [AllowNull()]$InitialValue,
        [Microsoft.Win32.RegistryValueKind]$InitialRegistryValueKind = [Microsoft.Win32.RegistryValueKind]::String,
        [Parameter(Mandatory = $true)][bool]$CheckExpandedValue,
        [AllowNull()]$ExpectedInitialExpandedValue,
        [Parameter(Mandatory = $true)][bool]$AssertRawAndExpandedDiffer
    )

    $configuration = New-RegistryPathTestPrefix -Name $Name
    try {
        if ($InitialExists) {
            Set-TestRegistryPathValue `
                -Configuration $configuration `
                -Value $InitialValue `
                -RegistryValueKind $InitialRegistryValueKind
        }
        else {
            Set-TestRegistryPathValue -Configuration $configuration -Delete
        }

        $stateArguments = @{
            Configuration = $configuration
            Label = "$Name initial"
            ExpectedExists = $InitialExists
            ExpectedValue = $InitialValue
            ExpectedRegistryValueKind = $InitialRegistryValueKind
        }
        if ($CheckExpandedValue) {
            $stateArguments.ExpectedExpandedValue = $ExpectedInitialExpandedValue
        }
        Assert-TestRegistryPathState @stateArguments
        if ($AssertRawAndExpandedDiffer) {
            $initialState = Get-TestRegistryPathState -Configuration $configuration
            Assert-True -Condition (-not [string]::Equals($initialState.Value, $initialState.ExpandedValue, [System.StringComparison]::Ordinal)) -Message "$Name initial PATH raw value unexpectedly equals its expanded view"
        }

        $result = Invoke-TestPathStoreRegression -Configuration $configuration
        Assert-Succeeded -Result $result -Label "$Name path-store rollback"
        Assert-TestRegistryPathState @stateArguments
        if ($AssertRawAndExpandedDiffer) {
            $restoredState = Get-TestRegistryPathState -Configuration $configuration
            Assert-True -Condition (-not [string]::Equals($restoredState.Value, $restoredState.ExpandedValue, [System.StringComparison]::Ordinal)) -Message "$Name restored PATH raw value unexpectedly equals its expanded view"
        }
    }
    finally {
        Remove-RegistryPathTestPrefix -Configuration $configuration
    }
}

function Invoke-RegistryOnlyRegression {
    $tokenName = 'TESSIVUM_REGISTRY_PATH_TEST_TOKEN'
    $previousToken = [Environment]::GetEnvironmentVariable($tokenName, [EnvironmentVariableTarget]::Process)
    $expandedToken = 'tessivum-registry-token-' + [System.Guid]::NewGuid().ToString('N')
    [Environment]::SetEnvironmentVariable($tokenName, $expandedToken, [EnvironmentVariableTarget]::Process)
    try {
        $registryStringValue = '%' + $tokenName + '%\literal;C:\registry-unrelated'
        Invoke-RegistryOnlyPathRoundTrip `
            -Name 'registry-only REG_SZ' `
            -InitialExists $true `
            -InitialValue $registryStringValue `
            -InitialRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::String) `
            -CheckExpandedValue $true `
            -ExpectedInitialExpandedValue $registryStringValue `
            -AssertRawAndExpandedDiffer $false

        $registryExpandValue = '%' + $tokenName + '%\expanded;C:\registry-unrelated'
        $registryExpandExpandedValue = $expandedToken + '\expanded;C:\registry-unrelated'
        Invoke-RegistryOnlyPathRoundTrip `
            -Name 'registry-only REG_EXPAND_SZ' `
            -InitialExists $true `
            -InitialValue $registryExpandValue `
            -InitialRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString) `
            -CheckExpandedValue $true `
            -ExpectedInitialExpandedValue $registryExpandExpandedValue `
            -AssertRawAndExpandedDiffer $true

        Invoke-RegistryOnlyPathRoundTrip `
            -Name 'registry-only empty REG_EXPAND_SZ' `
            -InitialExists $true `
            -InitialValue '' `
            -InitialRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString) `
            -CheckExpandedValue $true `
            -ExpectedInitialExpandedValue '' `
            -AssertRawAndExpandedDiffer $false

        Invoke-RegistryOnlyPathRoundTrip `
            -Name 'registry-only missing PATH' `
            -InitialExists $false `
            -InitialValue $null `
            -InitialRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::String) `
            -CheckExpandedValue $false `
            -ExpectedInitialExpandedValue $null `
            -AssertRawAndExpandedDiffer $false
    }
    finally {
        [Environment]::SetEnvironmentVariable($tokenName, $previousToken, [EnvironmentVariableTarget]::Process)
    }
}

function Invoke-RegistryInstallRollback {
    param(
        [Parameter(Mandatory = $true)][string]$FixtureArchive,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$InitialValue,
        [Parameter(Mandatory = $true)][string]$ExpectedExpandedValue
    )

    $configuration = New-RegistryPathTestPrefix -Name 'reg rollback'
    try {
        Set-TestRegistryPathValue `
            -Configuration $configuration `
            -Value $InitialValue `
            -RegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString)
        Assert-TestRegistryPathState `
            -Configuration $configuration `
            -Label 'registry install rollback initial' `
            -ExpectedExists $true `
            -ExpectedValue $InitialValue `
            -ExpectedRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString) `
            -ExpectedExpandedValue $ExpectedExpandedValue

        $installBase = Split-Path -Parent $configuration.InstallRoot
        [System.IO.Directory]::CreateDirectory($installBase) | Out-Null
        $lockPath = Join-Path -Path $installBase -ChildPath '.tessivum-installer.lock'
        [System.IO.File]::WriteAllText($lockPath, '', $script:Utf8NoBom)
        $originalInstallBaseAcl = Get-Acl -LiteralPath $installBase
        $installBaseAclChanged = $false
        try {
            $updatedInstallBaseAcl = Get-Acl -LiteralPath $installBase
            $denyCreateFiles = [System.Security.AccessControl.FileSystemAccessRule]::new(
                [System.Security.Principal.WindowsIdentity]::GetCurrent().User,
                [System.Security.AccessControl.FileSystemRights]::CreateFiles,
                [System.Security.AccessControl.InheritanceFlags]::None,
                [System.Security.AccessControl.PropagationFlags]::None,
                [System.Security.AccessControl.AccessControlType]::Deny
            )
            [void]$updatedInstallBaseAcl.AddAccessRule($denyCreateFiles)
            Set-Acl -LiteralPath $installBase -AclObject $updatedInstallBaseAcl
            $installBaseAclChanged = $true

            $rollbackResult = Invoke-TestInstall -Configuration $configuration -FixtureArchive $FixtureArchive -Version $Version
            Assert-Failed -Result $rollbackResult -Label 'registry install rollback'
            Assert-True -Condition (-not (Get-ResultText -Result $rollbackResult).Contains('rollback failed:')) -Message 'registry install rollback did not complete'
            Assert-TestRegistryPathState `
                -Configuration $configuration `
                -Label 'registry install rollback' `
                -ExpectedExists $true `
                -ExpectedValue $InitialValue `
                -ExpectedRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString) `
                -ExpectedExpandedValue $ExpectedExpandedValue
            $rollbackState = Get-TestRegistryPathState -Configuration $configuration
            Assert-True -Condition (-not [string]::Equals($rollbackState.Value, $rollbackState.ExpandedValue, [System.StringComparison]::Ordinal)) -Message 'registry install rollback PATH raw value unexpectedly equals its expanded view'
            Assert-NoPartialInstall -Configuration $configuration -Version $Version
        }
        finally {
            if ($installBaseAclChanged) {
                Set-Acl -LiteralPath $installBase -AclObject $originalInstallBaseAcl
            }
        }
    }
    finally {
        Remove-RegistryPathTestPrefix -Configuration $configuration
    }
}

function Invoke-RegistryNotifierAddTypeRollback {
    param(
        [Parameter(Mandatory = $true)][string]$FixtureArchive,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$InitialValue,
        [Parameter(Mandatory = $true)][string]$ExpectedInitialExpandedValue
    )

    $configuration = New-RegistryPathTestPrefix -Name 'notify'
    try {
        Set-TestRegistryPathValue `
            -Configuration $configuration `
            -Value $InitialValue `
            -RegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString)
        Assert-TestRegistryPathState `
            -Configuration $configuration `
            -Label 'notifier Add-Type initial' `
            -ExpectedExists $true `
            -ExpectedValue $InitialValue `
            -ExpectedRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString) `
            -ExpectedExpandedValue $ExpectedInitialExpandedValue
        $initialSnapshot = Get-TestRegistryPathState -Configuration $configuration
        $initialStateArguments = @{
            Configuration = $configuration
            Label = 'notifier Add-Type initial snapshot'
            ExpectedExists = [bool]$initialSnapshot.Exists
            ExpectedValue = $initialSnapshot.Value
            ExpectedRegistryValueKind = $initialSnapshot.RegistryValueKind
            ExpectedExpandedValue = $initialSnapshot.ExpandedValue
        }

        $versionPath = Join-Path -Path $configuration.InstallRoot -ChildPath $Version
        $canonicalPath = Join-Path -Path $configuration.BinDirectory -ChildPath 'tessivum.cmd'
        $aliasPath = Join-Path -Path $configuration.BinDirectory -ChildPath 'tsv.cmd'
        $ownershipPath = Join-Path -Path (Split-Path -Parent $configuration.InstallRoot) -ChildPath '.tessivum-path-owner'

        $installFailure = Invoke-TestInstall `
            -Configuration $configuration `
            -FixtureArchive $FixtureArchive `
            -Version $Version `
            -FailNotifierAddType
        Assert-Failed -Result $installFailure -Label 'notifier Add-Type install rollback'
        $installFailureText = Get-ResultText -Result $installFailure
        Assert-True -Condition $installFailureText.Contains('injected notifier Add-Type initialization failure') -Message ('notifier Add-Type install failure was suppressed: ' + $installFailureText)
        Assert-True -Condition (-not $installFailureText.Contains('rollback failed:')) -Message 'notifier Add-Type install rollback did not complete'
        $initialStateArguments['Label'] = 'notifier Add-Type install rollback PATH'
        Assert-TestRegistryPathState @initialStateArguments
        Assert-True -Condition (-not (Test-Path -LiteralPath $canonicalPath)) -Message 'notifier Add-Type install rollback left a canonical launcher'
        Assert-True -Condition (-not (Test-Path -LiteralPath $aliasPath)) -Message 'notifier Add-Type install rollback left an alias launcher'
        Assert-True -Condition (-not (Test-Path -LiteralPath $versionPath)) -Message 'notifier Add-Type install rollback left a managed version'
        Assert-True -Condition (-not (Test-Path -LiteralPath $ownershipPath)) -Message 'notifier Add-Type install rollback left a PATH ownership record'
        Assert-NoPartialInstall -Configuration $configuration -Version $Version

        $installResult = Invoke-TestInstall -Configuration $configuration -FixtureArchive $FixtureArchive -Version $Version
        Assert-Succeeded -Result $installResult -Label 'notifier Add-Type uninstall rollback setup install'
        $installedValue = Get-ExpectedInstalledPathValue -CurrentValue $InitialValue -BinDirectory $configuration.BinDirectory
        $installedExpandedValue = Get-ExpectedInstalledPathValue -CurrentValue $ExpectedInitialExpandedValue -BinDirectory $configuration.BinDirectory
        Assert-TestRegistryPathState `
            -Configuration $configuration `
            -Label 'notifier Add-Type uninstall rollback setup PATH' `
            -ExpectedExists $true `
            -ExpectedValue $installedValue `
            -ExpectedRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString) `
            -ExpectedExpandedValue $installedExpandedValue
        $installedSnapshot = Get-TestRegistryPathState -Configuration $configuration
        $installedStateArguments = @{
            Configuration = $configuration
            Label = 'notifier Add-Type uninstall rollback setup snapshot'
            ExpectedExists = [bool]$installedSnapshot.Exists
            ExpectedValue = $installedSnapshot.Value
            ExpectedRegistryValueKind = $installedSnapshot.RegistryValueKind
            ExpectedExpandedValue = $installedSnapshot.ExpandedValue
        }
        Assert-True -Condition ([System.IO.Directory]::Exists($versionPath)) -Message 'notifier Add-Type uninstall rollback setup is missing its managed version'
        Assert-True -Condition ([System.IO.File]::Exists($ownershipPath)) -Message 'notifier Add-Type uninstall rollback setup is missing its PATH ownership record'
        $ownershipContent = [System.IO.File]::ReadAllText($ownershipPath, $script:Utf8NoBom)

        $uninstallFailure = Invoke-TestUninstall -Configuration $configuration -FailNotifierAddType
        Assert-Failed -Result $uninstallFailure -Label 'notifier Add-Type uninstall rollback'
        $uninstallFailureText = Get-ResultText -Result $uninstallFailure
        Assert-True -Condition $uninstallFailureText.Contains('injected notifier Add-Type initialization failure') -Message ('notifier Add-Type uninstall failure was suppressed: ' + $uninstallFailureText)
        Assert-True -Condition (-not $uninstallFailureText.Contains('rollback failed:')) -Message 'notifier Add-Type uninstall rollback did not complete'
        $installedStateArguments['Label'] = 'notifier Add-Type uninstall rollback PATH'
        Assert-TestRegistryPathState @installedStateArguments
        Assert-StableLaunchers -Configuration $configuration -ExpectedVersion $Version -Label 'notifier Add-Type uninstall rollback'
        Assert-True -Condition ([System.IO.Directory]::Exists($versionPath)) -Message 'notifier Add-Type uninstall rollback did not restore the managed version'
        Assert-True -Condition ([System.IO.File]::Exists($ownershipPath)) -Message 'notifier Add-Type uninstall rollback did not restore the PATH ownership record'
        Assert-Equal -Expected $ownershipContent -Actual ([System.IO.File]::ReadAllText($ownershipPath, $script:Utf8NoBom)) -Message 'notifier Add-Type uninstall rollback changed the PATH ownership record'

        Wait-ForExclusiveFileAccess `
            -Path (Join-Path -Path $versionPath -ChildPath 'libexec\tessivum.exe') `
            -Label 'notifier Add-Type uninstall rollback launcher'
        Wait-ForDirectoryMove `
            -Path $versionPath `
            -Label 'notifier Add-Type uninstall rollback version directory'


        $uninstallResult = Invoke-TestUninstall -Configuration $configuration
        Assert-Succeeded -Result $uninstallResult -Label 'notifier Add-Type uninstall rollback cleanup'
        $initialStateArguments['Label'] = 'notifier Add-Type uninstall rollback cleanup PATH'
        Assert-TestRegistryPathState @initialStateArguments
    }
    finally {
        Remove-RegistryPathTestPrefix -Configuration $configuration
    }
}

function Invoke-RegistryPathRegression {
    param(
        [Parameter(Mandatory = $true)][string]$FixtureArchive,
        [Parameter(Mandatory = $true)][string]$Version
    )

    $tokenName = 'TESSIVUM_REGISTRY_PATH_TEST_TOKEN'
    $previousToken = [Environment]::GetEnvironmentVariable($tokenName, [EnvironmentVariableTarget]::Process)
    $expandedToken = 'tessivum-registry-token-' + [System.Guid]::NewGuid().ToString('N')
    [Environment]::SetEnvironmentVariable($tokenName, $expandedToken, [EnvironmentVariableTarget]::Process)
    try {
        $registryStringValue = '%' + $tokenName + '%\literal;C:\registry-unrelated'
        Invoke-RegistryPathRoundTrip `
            -Name 'reg sz' `
            -FixtureArchive $FixtureArchive `
            -Version $Version `
            -InitialExists $true `
            -InitialValue $registryStringValue `
            -InitialRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::String) `
            -CheckExpandedValue $true `
            -ExpectedInitialExpandedValue $registryStringValue `
            -AssertRawAndExpandedDiffer $false `
            -ExerciseUninstallRollback $false

        $registryExpandValue = '%' + $tokenName + '%\expanded;C:\registry-unrelated'
        $registryExpandExpandedValue = $expandedToken + '\expanded;C:\registry-unrelated'
        Invoke-RegistryNotifierAddTypeRollback `
            -FixtureArchive $FixtureArchive `
            -Version $Version `
            -InitialValue $registryExpandValue `
            -ExpectedInitialExpandedValue $registryExpandExpandedValue

        Invoke-RegistryInstallRollback `
            -FixtureArchive $FixtureArchive `
            -Version $Version `
            -InitialValue $registryExpandValue `
            -ExpectedExpandedValue $registryExpandExpandedValue

        Invoke-RegistryPathRoundTrip `
            -Name 'reg expand' `
            -FixtureArchive $FixtureArchive `
            -Version $Version `
            -InitialExists $true `
            -InitialValue $registryExpandValue `
            -InitialRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString) `
            -CheckExpandedValue $true `
            -ExpectedInitialExpandedValue $registryExpandExpandedValue `
            -AssertRawAndExpandedDiffer $true `
            -ExerciseUninstallRollback $true

        Invoke-RegistryPathRoundTrip `
            -Name 'reg empty' `
            -FixtureArchive $FixtureArchive `
            -Version $Version `
            -InitialExists $true `
            -InitialValue '' `
            -InitialRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::ExpandString) `
            -CheckExpandedValue $true `
            -ExpectedInitialExpandedValue '' `
            -AssertRawAndExpandedDiffer $false `
            -ExerciseUninstallRollback $false

        Invoke-RegistryPathRoundTrip `
            -Name 'reg missing' `
            -FixtureArchive $FixtureArchive `
            -Version $Version `
            -InitialExists $false `
            -InitialValue $null `
            -InitialRegistryValueKind ([Microsoft.Win32.RegistryValueKind]::String) `
            -CheckExpandedValue $false `
            -ExpectedInitialExpandedValue $null `
            -AssertRawAndExpandedDiffer $false `
            -ExerciseUninstallRollback $false `
            -UseMissingInstallPathCleanup $true
    }
    finally {
        [Environment]::SetEnvironmentVariable($tokenName, $previousToken, [EnvironmentVariableTarget]::Process)
    }
}

function Set-CurrentPowerShellHost {
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
}

function Get-WindowsPowerShell51Host {
    $candidateHost = Join-Path -Path $env:SystemRoot -ChildPath 'System32\WindowsPowerShell\v1.0\powershell.exe'
    Assert-True -Condition ([System.IO.File]::Exists($candidateHost)) -Message 'Windows PowerShell 5.1 is not available'
    return $candidateHost
}

function Get-PowerShell7Host {
    if ($PSVersionTable.PSEdition -eq 'Core' -and $PSVersionTable.PSVersion.Major -eq 7) {
        $currentHost = Join-Path -Path $PSHOME -ChildPath 'pwsh.exe'
        if ([System.IO.File]::Exists($currentHost)) {
            return $currentHost
        }
    }

    $command = Get-Command -Name 'pwsh.exe' -CommandType Application -ErrorAction SilentlyContinue
    if ($null -ne $command -and [System.IO.File]::Exists($command.Path)) {
        return $command.Path
    }

    $candidateHost = Join-Path -Path $env:ProgramFiles -ChildPath 'PowerShell\7\pwsh.exe'
    Assert-True -Condition ([System.IO.File]::Exists($candidateHost)) -Message 'PowerShell 7 is not available'
    return $candidateHost
}

function Invoke-RegistryOnlyAcrossPowerShellHosts {
    $hostSpecifications = @(
        [PSCustomObject]@{
            Path = (Get-WindowsPowerShell51Host)
            MajorVersion = 5
            Label = 'Windows PowerShell 5.1 registry-only PATH regression'
        },
        [PSCustomObject]@{
            Path = (Get-PowerShell7Host)
            MajorVersion = 7
            Label = 'PowerShell 7 registry-only PATH regression'
        }
    )
    foreach ($hostSpecification in $hostSpecifications) {
        $arguments = @(
            '-NoProfile',
            '-ExecutionPolicy',
            'Bypass',
            '-File',
            $script:TestScriptPath,
            '-RegistryOnly',
            '-InstallerPath',
            $script:InstallerPath,
            '-ExpectedPowerShellMajor',
            [string]$hostSpecification.MajorVersion
        )
        $global:LASTEXITCODE = 1
        $output = & $hostSpecification.Path @arguments 2>&1
        $result = [PSCustomObject]@{
            ExitCode = $LASTEXITCODE
            Output = $output
        }
        Assert-Succeeded -Result $result -Label $hostSpecification.Label
    }
}

function Invoke-RegistryPathRegressionAcrossPowerShellHosts {
    param([Parameter(Mandatory = $true)][string]$FixtureArchive)

    $hostSpecifications = @(
        [PSCustomObject]@{
            Path = (Get-WindowsPowerShell51Host)
            MajorVersion = 5
            Label = 'Windows PowerShell 5.1 registry PATH regression'
        },
        [PSCustomObject]@{
            Path = (Get-PowerShell7Host)
            MajorVersion = 7
            Label = 'PowerShell 7 registry PATH regression'
        }
    )
    foreach ($hostSpecification in $hostSpecifications) {
        $arguments = @(
            '-NoProfile',
            '-ExecutionPolicy',
            'Bypass',
            '-File',
            $script:TestScriptPath,
            '-ArchivePath',
            $FixtureArchive,
            '-InstallerPath',
            $script:InstallerPath,
            '-RegistryPathRegression',
            '-ExpectedPowerShellMajor',
            [string]$hostSpecification.MajorVersion
        )
        $global:LASTEXITCODE = 1
        $output = & $hostSpecification.Path @arguments 2>&1
        $result = [PSCustomObject]@{
            ExitCode = $LASTEXITCODE
            Output = $output
        }
        Assert-Succeeded -Result $result -Label $hostSpecification.Label
    }
}

try {
    if ($PSVersionTable.PSVersion.Major -lt 5) {
        Fail 'PowerShell 5.1 or later is required'
    }
    if ([Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
        Fail 'this test only supports Windows'
    }

    $script:InstallerPath = if ([string]::IsNullOrEmpty($InstallerPath)) {
        Join-Path -Path (Split-Path -Parent $PSScriptRoot) -ChildPath 'install.ps1'
    }
    else {
        Get-AbsolutePath -Path $InstallerPath
    }
    Assert-True -Condition ([System.IO.File]::Exists($script:InstallerPath)) -Message "installer does not exist: $script:InstallerPath"

    Set-CurrentPowerShellHost
    if ($ExpectedPowerShellMajor -gt 0) {
        Assert-Equal -Expected $ExpectedPowerShellMajor -Actual $PSVersionTable.PSVersion.Major -Message 'registry regression PowerShell major version'
    }

    # Leave room for the release tree beneath PowerShell 5.1's MAX_PATH boundary.
    $temporaryCandidate = Join-Path -Path ([System.IO.Path]::GetTempPath()) -ChildPath ('ti ' + [System.IO.Path]::GetRandomFileName())
    Assert-True -Condition (-not [System.IO.Directory]::Exists($temporaryCandidate) -and -not [System.IO.File]::Exists($temporaryCandidate)) -Message 'temporary test path already exists'
    $script:WorkDirectory = $temporaryCandidate
    [System.IO.Directory]::CreateDirectory($script:WorkDirectory) | Out-Null
    if ($RegistryOnly.IsPresent) {
        if ($ExpectedPowerShellMajor -gt 0) {
            Invoke-RegistryOnlyRegression
        }
        else {
            Invoke-RegistryOnlyAcrossPowerShellHosts
        }
        Write-Output 'Windows installer registry-only PATH regressions passed'
        return
    }

    $archive = Get-AbsolutePath -Path $ArchivePath
    Assert-True -Condition ([System.IO.File]::Exists($archive)) -Message "archive does not exist: $archive"
    Assert-True -Condition ([System.IO.File]::Exists($archive + '.sha256')) -Message "archive checksum does not exist: $archive.sha256"
    $release = Get-ReleaseArchiveInfo -Path $archive
    if ($RegistryPathRegression.IsPresent) {
        if ($ExpectedPowerShellMajor -gt 0) {
            Invoke-RegistryPathRegression -FixtureArchive $archive -Version $release.Version
        }
        else {
            Invoke-RegistryPathRegressionAcrossPowerShellHosts -FixtureArchive $archive
        }
        Write-Output 'Windows installer registry PATH regressions passed'
        return
    }

    Invoke-RegistryPathRegressionAcrossPowerShellHosts -FixtureArchive $archive

    $variants = Get-VersionVariants -SourceVersion $release.Version
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
