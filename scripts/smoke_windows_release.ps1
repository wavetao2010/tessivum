#Requires -Version 7.4
[CmdletBinding()]
param(
  [Parameter(Mandatory)]
  [ValidateNotNullOrEmpty()]
  [string] $ArchivePath,

  [Parameter(Mandatory)]
  [ValidateNotNullOrEmpty()]
  [string] $Version,

  [Parameter(Mandatory)]
  [ValidateNotNullOrEmpty()]
  [string] $RepositoryRoot,

  [Parameter(Mandatory)]
  [ValidateNotNullOrEmpty()]
  [string] $LogDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$script:Utf8 = [System.Text.UTF8Encoding]::new($false)
$script:ActiveServers = [System.Collections.Generic.List[object]]::new()
$script:ResourceVariables = @(
  'TESSIVUM_COMPAT_HOST',
  'TESSIVUM_HOST_MODULE_ROOT',
  'TESSIVUM_MARKET_TARBALL',
  'TESSIVUM_MARKET_SHA256_FILE',
  'TESSIVUM_MARKET_SOURCE_FILE',
  'CORDIS_VENDOR_ROOT'
)

function Assert-Release {
  param(
    [Parameter(Mandatory)]
    [bool] $Condition,

    [Parameter(Mandatory)]
    [string] $Message
  )

  if (-not $Condition) {
    throw $Message
  }
}

function Write-ReleaseText {
  param(
    [Parameter(Mandatory)]
    [string] $Name,

    [AllowNull()]
    [string] $Text
  )

  [System.IO.File]::WriteAllText(
    (Join-Path $script:LogDirectory $Name),
    $Text,
    $script:Utf8
  )
}

function Write-ReleaseLines {
  param(
    [Parameter(Mandatory)]
    [string] $Name,

    [AllowEmptyCollection()]
    [object[]] $Lines
  )

  $text = (@($Lines | ForEach-Object { [string] $_ }) -join [Environment]::NewLine)
  if ($text.Length -gt 0) {
    $text += [Environment]::NewLine
  }
  Write-ReleaseText -Name $Name -Text $text
}

function Assert-NoReparsePoints {
  param(
    [Parameter(Mandatory)]
    [string] $Root
  )

  $items = @(
    Get-Item -LiteralPath $Root -Force
  ) + @(
    Get-ChildItem -LiteralPath $Root -Force -Recurse
  )
  foreach ($item in $items) {
    if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
      throw "Release payload contains a reparse point: $($item.FullName)"
    }
  }
}

function Expand-ValidatedReleaseArchive {
  param(
    [Parameter(Mandatory)]
    [string] $Archive,

    [Parameter(Mandatory)]
    [string] $Destination,

    [Parameter(Mandatory)]
    [string] $ExpectedRoot
  )

  Add-Type -AssemblyName System.IO.Compression.FileSystem
  $zip = [System.IO.Compression.ZipFile]::OpenRead($Archive)
  try {
    $entries = @($zip.Entries)
    Assert-Release ($entries.Count -gt 0) 'Windows release ZIP is empty.'
    $seen = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::Ordinal)
    $roots = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::Ordinal)
    foreach ($entry in $entries) {
      $fullName = [string] $entry.FullName
      Assert-Release (
        $fullName.Length -gt 0 -and
        -not $fullName.StartsWith('/') -and
        -not $fullName.StartsWith('\') -and
        -not ($fullName -match '^[A-Za-z]:') -and
        -not $fullName.Contains('\') -and
        -not $fullName.Contains('//')
      ) "Windows release ZIP has an absolute or malformed entry: $fullName"

      $relative = $fullName.TrimEnd([char[]]'/')
      Assert-Release ($relative.Length -gt 0) "Windows release ZIP has an empty entry: $fullName"
      $parts = @($relative.Split('/'))
      Assert-Release (
        @($parts | Where-Object { $_ -eq '' -or $_ -eq '.' -or $_ -eq '..' }).Count -eq 0
      ) "Windows release ZIP has an unsafe entry: $fullName"
      Assert-Release ($parts[0] -eq $ExpectedRoot) "Windows release ZIP escapes ${ExpectedRoot}: $fullName"
      Assert-Release (
        $parts.Count -ne 1 -or $fullName.EndsWith('/')
      ) "Windows release ZIP root is not a directory: $fullName"
      $unixMode = (($entry.ExternalAttributes -shr 16) -band 0xFFFF)
      Assert-Release (
        (($unixMode -band 0xF000) -ne 0xA000)
      ) "Windows release ZIP contains a symbolic link: $fullName"
      Assert-Release ($seen.Add($relative)) "Windows release ZIP has a duplicate entry: $fullName"
      [void] $roots.Add($parts[0])
    }
    Assert-Release (
      $roots.Count -eq 1 -and $roots.Contains($ExpectedRoot)
    ) "Windows release ZIP must have exactly one root: $ExpectedRoot"
  } finally {
    $zip.Dispose()
  }

  [System.IO.Compression.ZipFile]::ExtractToDirectory($Archive, $Destination)
  $root = Join-Path $Destination $ExpectedRoot
  Assert-Release (Test-Path -LiteralPath $root -PathType Container) "Windows release ZIP did not extract $ExpectedRoot."
  Assert-NoReparsePoints -Root $root
  return $root
}

function Invoke-PackagedLauncher {
  param(
    [Parameter(Mandatory)]
    [string] $Launcher,

    [Parameter(Mandatory)]
    [string[]] $Arguments,

    [Parameter(Mandatory)]
    [string] $LogName
  )

  $saved = @{}
  $output = @()
  $nativeExit = -1
  # Capture stderr before the explicit exit-code check below raises the failure.
  $PSNativeCommandUseErrorActionPreference = $false
  $failure = $null
  try {
    foreach ($name in $script:ResourceVariables) {
      $saved[$name] = [Environment]::GetEnvironmentVariable(
        $name,
        [EnvironmentVariableTarget]::Process
      )
      [Environment]::SetEnvironmentVariable(
        $name,
        $null,
        [EnvironmentVariableTarget]::Process
      )
    }
    $output = @(& $Launcher @Arguments 2>&1)
    $nativeExit = $LASTEXITCODE
  } catch {
    $failure = $_
  } finally {
    foreach ($name in $script:ResourceVariables) {
      [Environment]::SetEnvironmentVariable(
        $name,
        $saved[$name],
        [EnvironmentVariableTarget]::Process
      )
    }
    Write-ReleaseLines -Name $LogName -Lines $output
  }
  if ($null -ne $failure) {
    throw $failure
  }
  if ($nativeExit -ne 0) {
    throw "Launcher failed with exit code $nativeExit; see $LogName."
  }
  return $output
}

function Set-ReleaseProcessEnvironmentVariable {
  param(
    [Parameter(Mandatory)]
    [System.Diagnostics.ProcessStartInfo] $Info,

    [Parameter(Mandatory)]
    [string] $Name,

    [AllowNull()]
    [object] $Value
  )

  # ProcessStartInfo preserves an inherited key's casing on assignment. Normalize
  # every name before applying an override so the child receives one Windows key.
  foreach ($existingName in @($Info.Environment.Keys)) {
    if ([string]::Equals(
      [string] $existingName,
      $Name,
      [System.StringComparison]::OrdinalIgnoreCase
    )) {
      [void] $Info.Environment.Remove([string] $existingName)
    }
  }
  if ($null -ne $Value) {
    $Info.Environment[$Name] = [string] $Value
  }
}

function Start-ReleaseServer {
  param(
    [Parameter(Mandatory)]
    [string] $Name,

    [Parameter(Mandatory)]
    [string] $DataDirectory,

    [Parameter(Mandatory)]
    [int] $Port,

    [hashtable] $EnvironmentOverrides = @{}
  )

  [System.IO.Directory]::CreateDirectory($DataDirectory) | Out-Null
  $info = [System.Diagnostics.ProcessStartInfo]::new()
  $info.FileName = $env:ComSpec
  $info.WorkingDirectory = $script:RepositoryRoot
  $info.UseShellExecute = $false
  $info.CreateNoWindow = $true
  $info.RedirectStandardOutput = $true
  $info.RedirectStandardError = $true
  $command = '"{0}" --data-dir "{1}" web' -f $script:Launcher, $DataDirectory
  $info.Arguments = '/d /s /c "' + $command + '"'

  $environmentNames = @(
    $script:ResourceVariables
    'TESSIVUM_ROOT'
    'DSH_HOME'
    'TESSIVUM_REMOTE_AUTO_TUNNEL'
    'TESSIVUM_REMOTE_TRUSTED_TUNNEL'
    'TESSIVUM_WEB_TRUSTED_AUTHORITIES'
    'TESSIVUM_REPLAY'
    'TESSIVUM_REPLAY_FILE'
    'TESSIVUM_REPLAY_OVERRIDE_FILE'
    'TESSIVUM_REPLAY_PACE_MS'
    'TESSIVUM_REPLAY_CONTEXT_WINDOW'
  )
  foreach ($environmentName in $environmentNames) {
    Set-ReleaseProcessEnvironmentVariable -Info $info -Name $environmentName -Value $null
  }
  foreach ($entry in $EnvironmentOverrides.GetEnumerator()) {
    Set-ReleaseProcessEnvironmentVariable -Info $info -Name ([string] $entry.Key) -Value $entry.Value
  }
  Set-ReleaseProcessEnvironmentVariable -Info $info -Name 'TESSIVUM_WEB_ADDR' -Value "127.0.0.1:$Port"
  Set-ReleaseProcessEnvironmentVariable -Info $info -Name 'TESSIVUM_REMOTE_ACCESS' -Value '0'

  $process = [System.Diagnostics.Process]::new()
  $process.StartInfo = $info
  Assert-Release ($process.Start()) "Could not start $Name Web server."
  $handle = [pscustomobject]@{
    Name = $Name
    Port = $Port
    Process = $process
    OwnedProcesses = [System.Collections.Generic.List[object]]::new()
    ListeningProcess = $null
    StandardOutput = $process.StandardOutput.ReadToEndAsync()
    StandardError = $process.StandardError.ReadToEndAsync()
    StandardOutputPath = Join-Path $script:LogDirectory "$Name.stdout.log"
    StandardErrorPath = Join-Path $script:LogDirectory "$Name.stderr.log"
    LogWritten = $false
  }
  [void] (Add-ReleaseServerProcess -Handle $handle -Process $process -Description "$Name launcher process")
  [void] $script:ActiveServers.Add($handle)
  return $handle
}

function New-ReleaseProcessIdentity {
  param(
    [Parameter(Mandatory)]
    [System.Diagnostics.Process] $Process,

    [Parameter(Mandatory)]
    [string] $Description
  )

  try {
    $processId = $Process.Id
    $startedAtTicks = $Process.StartTime.ToUniversalTime().Ticks
  } catch {
    throw "Could not capture process identity for ${Description}: $($_.Exception.Message)"
  }
  return [pscustomobject]@{
    Description = $Description
    Process = $Process
    ProcessId = $processId
    StartedAtTicks = $startedAtTicks
  }
}

function Test-ReleaseProcessIdentity {
  param(
    [Parameter(Mandatory)]
    [object] $Identity
  )

  try {
    return (
      -not $Identity.Process.HasExited -and
      $Identity.Process.Id -eq $Identity.ProcessId -and
      $Identity.Process.StartTime.ToUniversalTime().Ticks -eq $Identity.StartedAtTicks
    )
  } catch {
    return $false
  }
}

function Add-ReleaseServerProcess {
  param(
    [Parameter(Mandatory)]
    [object] $Handle,

    [Parameter(Mandatory)]
    [System.Diagnostics.Process] $Process,

    [Parameter(Mandatory)]
    [string] $Description,

    [switch] $Listening
  )

  $identity = New-ReleaseProcessIdentity -Process $Process -Description $Description
  foreach ($existing in $Handle.OwnedProcesses) {
    if (
      $existing.ProcessId -eq $identity.ProcessId -and
      $existing.StartedAtTicks -eq $identity.StartedAtTicks
    ) {
      if (-not [object]::ReferenceEquals($existing.Process, $Process)) {
        $Process.Dispose()
      }
      if ($Listening) {
        $Handle.ListeningProcess = $existing
      }
      return $existing
    }
  }
  [void] $Handle.OwnedProcesses.Add($identity)
  if ($Listening) {
    $Handle.ListeningProcess = $identity
  }
  return $identity
}

function Get-ReleaseProcess {
  param(
    [Parameter(Mandatory)]
    [int] $ProcessId
  )

  try {
    $process = [System.Diagnostics.Process]::GetProcessById($ProcessId)
    if ($process.HasExited) {
      $process.Dispose()
      return $null
    }
    return $process
  } catch [System.ArgumentException] {
    return $null
  }
}

function Get-ListeningProcessId {
  param(
    [Parameter(Mandatory)]
    [int] $Port
  )

  $listeners = @(
    Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue |
      Where-Object { $_.LocalAddress -eq '127.0.0.1' }
  )
  if ($listeners.Count -gt 1) {
    throw "More than one process is listening on 127.0.0.1:$Port."
  }
  if ($listeners.Count -eq 1) {
    return [int] $listeners[0].OwningProcess
  }
  return $null
}

function Get-ListeningReleaseProcess {
  param(
    [Parameter(Mandatory)]
    [int] $Port
  )

  $processId = Get-ListeningProcessId -Port $Port
  if ($null -eq $processId) {
    return $null
  }
  $process = Get-ReleaseProcess -ProcessId $processId
  if ($null -eq $process) {
    return $null
  }
  $confirmedProcessId = Get-ListeningProcessId -Port $Port
  if ($confirmedProcessId -ne $processId) {
    $process.Dispose()
    return $null
  }
  return $process
}

function Test-PackagedTessivumProcess {
  param(
    [Parameter(Mandatory)]
    [System.Diagnostics.Process] $Process
  )

  try {
    if ($Process.HasExited) {
      return $false
    }
    $path = [string] $Process.Path
    return -not [string]::IsNullOrWhiteSpace($path) -and
      [string]::Equals(
        [System.IO.Path]::GetFullPath($path),
        $script:Executable,
        [System.StringComparison]::OrdinalIgnoreCase
      )
  } catch {
    return $false
  }
}

function Register-ListeningReleaseServerProcess {
  param(
    [Parameter(Mandatory)]
    [object] $Handle
  )

  $process = Get-ListeningReleaseProcess -Port $Handle.Port
  if ($null -eq $process) {
    return $null
  }
  $processId = $process.Id
  if (-not (Test-PackagedTessivumProcess -Process $process)) {
    $process.Dispose()
    throw "A non-packaged process ($processId) is listening on port $($Handle.Port)."
  }
  return Add-ReleaseServerProcess -Handle $Handle -Process $process -Description "$($Handle.Name) listening process" -Listening
}

function Get-AvailableLoopbackPort {
  $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
  try {
    $listener.Start()
    $endpoint = [System.Net.IPEndPoint] $listener.LocalEndpoint
    return $endpoint.Port
  } finally {
    $listener.Stop()
  }
}

function Wait-ForProcessExit {
  param(
    [Parameter(Mandatory)]
    [System.Diagnostics.Process] $Process,

    [Parameter(Mandatory)]
    [string] $Description,

    [int] $TimeoutSeconds = 15
  )

  $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
  while ([DateTime]::UtcNow -lt $deadline) {
    if ($Process.HasExited) {
      return
    }
    Start-Sleep -Milliseconds 100
  }
  throw "$Description did not exit within $TimeoutSeconds seconds."
}

function Finalize-ReleaseServerLogs {
  param(
    [Parameter(Mandatory)]
    [object] $Handle
  )

  if ($Handle.LogWritten) {
    return
  }
  $stdout = ''
  $stderr = ''
  try {
    if ($Handle.StandardOutput.Wait(10000)) {
      $stdout = $Handle.StandardOutput.GetAwaiter().GetResult()
    } else {
      $stdout = 'stdout capture did not close after process cleanup.'
    }
  } catch {
    $stdout = "stdout capture failed: $($_.Exception.Message)"
  }
  try {
    if ($Handle.StandardError.Wait(10000)) {
      $stderr = $Handle.StandardError.GetAwaiter().GetResult()
    } else {
      $stderr = 'stderr capture did not close after process cleanup.'
    }
  } catch {
    $stderr = "stderr capture failed: $($_.Exception.Message)"
  }
  [System.IO.File]::WriteAllText($Handle.StandardOutputPath, $stdout, $script:Utf8)
  [System.IO.File]::WriteAllText($Handle.StandardErrorPath, $stderr, $script:Utf8)
  $Handle.LogWritten = $true
}

function Get-ReleaseServerLogs {
  param(
    [Parameter(Mandatory)]
    [object] $Handle
  )

  $stdout = if (Test-Path -LiteralPath $Handle.StandardOutputPath -PathType Leaf) {
    [System.IO.File]::ReadAllText($Handle.StandardOutputPath, $script:Utf8)
  } else {
    ''
  }
  $stderr = if (Test-Path -LiteralPath $Handle.StandardErrorPath -PathType Leaf) {
    [System.IO.File]::ReadAllText($Handle.StandardErrorPath, $script:Utf8)
  } else {
    ''
  }
  return "$stdout$stderr"
}


function Force-StopReleaseServer {
  param(
    [Parameter(Mandatory)]
    [object] $Handle
  )

  try {
    # Only direct handles captured at start/readiness/restart may be terminated.
    foreach ($identity in @($Handle.OwnedProcesses)) {
      if (-not (Test-ReleaseProcessIdentity -Identity $identity)) {
        continue
      }
      try {
        $identity.Process.Kill($true)
      } catch [System.InvalidOperationException] {
        # The owned process exited between generation validation and termination.
      }
    }
    foreach ($identity in @($Handle.OwnedProcesses)) {
      if (Test-ReleaseProcessIdentity -Identity $identity) {
        Wait-ForProcessExit -Process $identity.Process -Description "$($identity.Description) ($($identity.ProcessId))" -TimeoutSeconds 10
      }
    }
  } finally {
    foreach ($identity in @($Handle.OwnedProcesses)) {
      try {
        $identity.Process.Dispose()
      } catch {
        # Process disposal is best-effort after bounded cleanup.
      }
    }
    Finalize-ReleaseServerLogs -Handle $Handle
    [void] $script:ActiveServers.Remove($Handle)
  }
}

function Wait-ForWeb {
  param(
    [Parameter(Mandatory)]
    [int] $Port,

    [Parameter(Mandatory)]
    [object] $Handle,

    [switch] $IgnoreTrackedExit,

    [object] $PreviousListener = $null
  )

  $lastFailure = 'No response received.'
  for ($attempt = 0; $attempt -lt 100; $attempt += 1) {
    try {
      $listener = Register-ListeningReleaseServerProcess -Handle $Handle
      if ($null -eq $listener) {
        $lastFailure = 'No packaged process is listening.'
      } elseif (
        $null -ne $PreviousListener -and
        $listener.ProcessId -eq $PreviousListener.ProcessId -and
        $listener.StartedAtTicks -eq $PreviousListener.StartedAtTicks
      ) {
        $lastFailure = "Restart still serves the original process $($listener.ProcessId)."
      } else {
        $response = Invoke-WebRequest -Uri "http://127.0.0.1:$Port/" -TimeoutSec 2
        if ($response.StatusCode -eq 200) {
          return [string] $response.Content
        }
        $lastFailure = "HTTP $($response.StatusCode)"
      }
    } catch {
      $lastFailure = $_.Exception.Message
    }
    if (-not $IgnoreTrackedExit -and $Handle.Process.HasExited) {
      break
    }
    Start-Sleep -Milliseconds 100
  }
  throw "Web server on port $Port did not become ready: $lastFailure. See $($Handle.StandardOutputPath) and $($Handle.StandardErrorPath)."
}

function Invoke-ReleaseJson {
  param(
    [Parameter(Mandatory)]
    [string] $Uri,

    [Parameter(Mandatory)]
    [hashtable] $Body,

    [hashtable] $Headers = @{}
  )

  $json = $Body | ConvertTo-Json -Depth 20 -Compress
  return Invoke-RestMethod -Method Post -Uri $Uri -ContentType 'application/json' -Headers $Headers -Body $json
}

function Invoke-ClientRequest {
  param(
    [Parameter(Mandatory)]
    [int] $Port,

    [Parameter(Mandatory)]
    [string] $Method,

    [Parameter(Mandatory)]
    [string] $RpcId,

    [Parameter(Mandatory)]
    [hashtable] $Payload
  )

  return Invoke-ReleaseJson -Uri "http://127.0.0.1:$Port/api/$Method" -Body @{
    type = 'client-request'
    rpcId = $RpcId
    method = $Method
    payload = $Payload
  }
}

function Get-PersistedReleaseSessionEvents {
  param(
    [Parameter(Mandatory)]
    [string] $SessionPath
  )

  $events = [System.Collections.Generic.List[object]]::new()
  foreach ($line in [System.IO.File]::ReadAllLines($SessionPath, $script:Utf8)) {
    if ([string]::IsNullOrWhiteSpace($line)) {
      continue
    }
    $record = $line | ConvertFrom-Json -Depth 100
    if ($null -ne $record.PSObject.Properties['seq']) {
      [void] $events.Add($record)
    }
  }
  return @($events)
}

function Assert-CliSmokeToolRoundTrip {
  param(
    [Parameter(Mandatory)]
    [string] $SessionPath,

    [Parameter(Mandatory)]
    [int] $ExpectedResults,

    [Parameter(Mandatory)]
    [string] $Phase
  )

  $events = @(Get-PersistedReleaseSessionEvents -SessionPath $SessionPath)
  $calls = @($events | Where-Object {
    $_.type -eq 'tool/call' -and $_.data.callId -eq 'cli-smoke-call'
  })
  Assert-Release ($calls.Count -eq $ExpectedResults) "Headless $Phase did not persist $ExpectedResults cli-smoke-call tool calls."
  foreach ($call in $calls) {
    Assert-Release ($call.data.name -eq 'bash') "Headless $Phase persisted cli-smoke-call with the wrong tool."
  }
  $results = @($events | Where-Object {
    $_.type -eq 'tool/result' -and
    $_.data.message.source.kind -eq 'tool' -and
    $_.data.message.source.callId -eq 'cli-smoke-call'
  })
  Assert-Release ($results.Count -eq $ExpectedResults) "Headless $Phase did not persist $ExpectedResults cli-smoke-call tool results."
  foreach ($result in $results) {
    $blocks = @($result.data.message.content | Where-Object {
      $_.type -eq 'tool-result' -and $_.toolCallId -eq 'cli-smoke-call'
    })
    Assert-Release ($blocks.Count -eq 1) "Headless $Phase persisted an invalid cli-smoke-call tool result."
    $toolResult = $blocks[0]
    $isError = $toolResult.PSObject.Properties['isError']
    Assert-Release (
      $null -ne $isError -and $isError.Value -eq $false
    ) "Headless $Phase recorded cli-smoke-call as a failed tool result."
    $toolText = @($toolResult.content | Where-Object {
      $_.type -eq 'text'
    } | ForEach-Object {
      [string] $_.text
    }) -join ''
    Assert-Release (
      $toolText.Contains('CLI_TOOL_ROUND_TRIP')
    ) "Headless $Phase cli-smoke-call result did not contain CLI_TOOL_ROUND_TRIP."
  }
}

function Get-ReleaseSessionEvents {
  param(
    [Parameter(Mandatory)]
    [int] $Port,

    [Parameter(Mandatory)]
    [string] $SessionId,

    [Parameter(Mandatory)]
    [string] $RpcId
  )

  $history = Invoke-ClientRequest -Port $Port -Method 'session.history' -RpcId $RpcId -Payload @{
    sessionId = $SessionId
    maxMessages = 1000
  }
  Assert-Release ($history.result.ok -eq $true) "Could not read PTC session history for $SessionId."
  return @($history.result.value.events | ForEach-Object {
    $_.event
  })
}

function Wait-ForReleaseSessionTurn {
  param(
    [Parameter(Mandatory)]
    [int] $Port,

    [Parameter(Mandatory)]
    [object] $Handle,

    [Parameter(Mandatory)]
    [string] $SessionId,

    [int] $ExpectedTurns = 1,

    [int] $TimeoutSeconds = 30
  )

  $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
  $events = @()
  $lastFailure = 'No completed turn observed.'
  while ([DateTime]::UtcNow -lt $deadline) {
    try {
      $events = @(Get-ReleaseSessionEvents -Port $Port -SessionId $SessionId -RpcId 'release-ptc-history')
      $turns = @($events | Where-Object {
        $_.type -eq 'turn/end'
      })
      if ($turns.Count -ge $ExpectedTurns) {
        return $events
      }
      $lastFailure = "Observed $($turns.Count) completed turns."
    } catch {
      $lastFailure = $_.Exception.Message
    }
    if ($Handle.Process.HasExited) {
      break
    }
    Start-Sleep -Milliseconds 100
  }
  throw "PTC session $SessionId did not complete within $TimeoutSeconds seconds: $lastFailure. See $($Handle.StandardOutputPath) and $($Handle.StandardErrorPath)."
}

function Assert-PtcRuntimeRoundTrip {
  param(
    [Parameter(Mandatory)]
    [object[]] $Events
  )

  $turnEnds = @($Events | Where-Object {
    $_.type -eq 'turn/end'
  })
  Assert-Release ($turnEnds.Count -eq 1) 'PTC smoke did not persist exactly one completed turn.'
  Assert-Release (
    $turnEnds[0].data.reason.kind -eq 'completed'
  ) 'PTC smoke turn did not complete successfully.'
  $runCodeCalls = @($Events | Where-Object {
    $_.type -eq 'tool/call' -and $_.data.callId -eq 'ptc-runtime-call'
  })
  Assert-Release ($runCodeCalls.Count -eq 1) 'PTC smoke did not invoke run_code exactly once.'
  Assert-Release (
    $runCodeCalls[0].data.name -eq 'run_code' -and
    ([string] $runCodeCalls[0].data.arguments).Contains('PTC_RUNTIME_ROUND_TRIP')
  ) 'PTC smoke did not persist the expected run_code request.'
  $outerResults = @($Events | Where-Object {
    $_.type -eq 'tool/result' -and
    $_.data.message.source.kind -eq 'tool' -and
    $_.data.message.source.callId -eq 'ptc-runtime-call'
  })
  Assert-Release ($outerResults.Count -eq 1) 'PTC smoke did not persist one run_code result.'
  $outerBlocks = @($outerResults[0].data.message.content | Where-Object {
    $_.type -eq 'tool-result' -and $_.toolCallId -eq 'ptc-runtime-call'
  })
  Assert-Release ($outerBlocks.Count -eq 1) 'PTC smoke persisted an invalid run_code result.'
  $outerIsError = $outerBlocks[0].PSObject.Properties['isError']
  Assert-Release (
    $null -ne $outerIsError -and $outerIsError.Value -eq $false
  ) 'PTC smoke recorded run_code as a failed tool result.'
  $outerText = @($outerBlocks[0].content | Where-Object {
    $_.type -eq 'text'
  } | ForEach-Object {
    [string] $_.text
  }) -join ''
  Assert-Release (
    $outerText.Contains('PTC_RUNTIME_ROUND_TRIP')
  ) 'PTC smoke run_code result did not contain PTC_RUNTIME_ROUND_TRIP.'
  $dispatches = @($Events | Where-Object {
    $_.type -eq 'tool/code-dispatch' -and
    $_.data.rootCallId -eq 'ptc-runtime-call'
  })
  Assert-Release ($dispatches.Count -eq 1) 'PTC smoke did not persist one nested shell dispatch.'
  $dispatch = $dispatches[0].data
  $dispatchIsError = $dispatch.PSObject.Properties['isError']
  Assert-Release (
    $dispatch.name -eq 'bash' -and
    $dispatch.status -eq 'completed' -and
    $null -ne $dispatchIsError -and $dispatchIsError.Value -eq $false
  ) 'PTC smoke nested shell dispatch failed.'
  $dispatchText = @($dispatch.content | Where-Object {
    $_.type -eq 'text'
  } | ForEach-Object {
    [string] $_.text
  }) -join ''
  Assert-Release (
    $dispatchText.Contains('PTC_RUNTIME_ROUND_TRIP')
  ) 'PTC smoke nested shell result did not contain PTC_RUNTIME_ROUND_TRIP.'
}

function Write-PtcRuntimeReplay {
  param(
    [Parameter(Mandatory)]
    [string] $Path
  )

  $code = 'const shell = await tools.bash({command:"echo PTC_RUNTIME_ROUND_TRIP",description:"Prove the PTC worker can execute a nested shell."}); if (!shell.text.includes("PTC_RUNTIME_ROUND_TRIP")) { throw new Error("PTC nested shell output is missing its marker"); } return {marker:"PTC_RUNTIME_ROUND_TRIP",output:shell.text};'
  $arguments = @{ description = 'Run the PTC worker through a nested shell.'; code = $code } | ConvertTo-Json -Compress
  $rows = @(
    @{ requestId = 'ptc-runtime-request-1'; chunk = @{ type = 'block-start'; index = 0; blockType = 'tool-call' } }
    @{ requestId = 'ptc-runtime-request-1'; chunk = @{ type = 'tool-call-delta'; index = 0; id = 'ptc-runtime-call'; name = 'run_code'; argumentsDelta = $arguments } }
    @{ requestId = 'ptc-runtime-request-1'; chunk = @{ type = 'block-end'; index = 0; block = @{ type = 'tool-call'; id = 'ptc-runtime-call'; name = 'run_code'; arguments = $arguments } } }
    @{ requestId = 'ptc-runtime-request-1'; chunk = @{ type = 'finish'; reason = @{ kind = 'tool-calls' } } }
    @{ requestId = 'ptc-runtime-request-2'; chunk = @{ type = 'block-start'; index = 0; blockType = 'text' } }
    @{ requestId = 'ptc-runtime-request-2'; chunk = @{ type = 'text-delta'; index = 0; text = 'PTC execution finished.' } }
    @{ requestId = 'ptc-runtime-request-2'; chunk = @{ type = 'block-end'; index = 0; block = @{ type = 'text'; text = 'PTC execution finished.' } } }
    @{ requestId = 'ptc-runtime-request-2'; chunk = @{ type = 'finish'; reason = @{ kind = 'stop' } } }
  )
  $replay = (@($rows | ForEach-Object {
    $_ | ConvertTo-Json -Compress -Depth 10
  }) -join "`n") + "`n"
  [System.IO.File]::WriteAllText($Path, $replay, $script:Utf8)
}

function Stop-ReleaseServer {
  param(
    [Parameter(Mandatory)]
    [object] $Handle,

    [switch] $RequireHostShutdown
  )

  try {
    if ($RequireHostShutdown) {
      $shutdown = Invoke-ReleaseJson -Uri "http://127.0.0.1:$($Handle.Port)/api/host/shutdown" -Body @{
        requestId = "release-shutdown-$($Handle.Name)"
        args = @{}
      }
      Assert-Release ($shutdown.ok -eq $true) "Host shutdown endpoint failed for $($Handle.Name)."
      # The RPC stops Host resources; the CLI listener has a separate lifetime.
      # Finally below terminates and waits for the retained process identities.
    }
  } finally {
    Force-StopReleaseServer -Handle $Handle
  }
  if ($RequireHostShutdown) {
    $logs = Get-ReleaseServerLogs -Handle $Handle
    Assert-Release (
      $logs -notmatch 'shutdown exceeded|SHUTDOWN_FAILED|HOST_SHUTDOWN_FAILED'
    ) "Host shutdown failed for $($Handle.Name); see $($Handle.StandardErrorPath)."
  }
}

function Invoke-ReleaseRestart {
  param(
    [Parameter(Mandatory)]
    [object] $Handle
  )

  $original = $Handle.ListeningProcess
  Assert-Release ($null -ne $original) "Release server $($Handle.Name) did not retain its original listener identity."
  $restart = Invoke-ReleaseJson -Uri "http://127.0.0.1:$($Handle.Port)/api/remoteAccess/configure" -Body @{
    requestId = "release-restart-$($Handle.Name)"
    args = @{ enabled = $false }
  }
  Assert-Release (
    $restart.ok -eq $true -and $restart.output.restarting -eq $true
  ) "Web restart request was not accepted for $($Handle.Name)."
  Wait-ForProcessExit -Process $Handle.Process -Description "Restarting release server $($Handle.Name)"
  Assert-Release ($Handle.Process.ExitCode -eq 0) "Restarting release server $($Handle.Name) exited unsuccessfully."
  [void] (Wait-ForWeb -Port $Handle.Port -Handle $Handle -IgnoreTrackedExit -PreviousListener $original)
  $restarted = $Handle.ListeningProcess
  Assert-Release ($null -ne $restarted) "Restarted release server did not retain a listener identity."
  Assert-Release (
    Test-ReleaseProcessIdentity -Identity $restarted
  ) 'Restarted release server process identity is no longer valid.'
  Assert-Release (
    -not (
      $restarted.ProcessId -eq $original.ProcessId -and
      $restarted.StartedAtTicks -eq $original.StartedAtTicks
    )
  ) 'Restart did not create a new release process generation.'
  Assert-Release (
    Test-PackagedTessivumProcess -Process $restarted.Process
  ) 'Restarted process does not run the packaged tessivum.exe.'
  return $restarted.Process
}

$locationPushed = $false
$smokeRoot = $null
try {
  $script:RepositoryRoot = (Resolve-Path -LiteralPath $RepositoryRoot -ErrorAction Stop).Path
  $archive = (Resolve-Path -LiteralPath $ArchivePath -ErrorAction Stop).Path
  $script:LogDirectory = [System.IO.Path]::GetFullPath($LogDirectory)
  [System.IO.Directory]::CreateDirectory($script:LogDirectory) | Out-Null
  $expectedArchive = "tessivum-$Version-x86_64-pc-windows-msvc.zip"
  Assert-Release (
    [System.IO.Path]::GetFileName($archive) -ceq $expectedArchive
  ) "Expected Windows release archive $expectedArchive, got $archive."
  $checksum = "$archive.sha256"
  Assert-Release (Test-Path -LiteralPath $checksum -PathType Leaf) "Missing Windows release checksum: $checksum"
  $actualHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
  $checksumText = [System.IO.File]::ReadAllText($checksum, $script:Utf8).TrimEnd([char[]]"`r`n")
  Assert-Release (
    $checksumText -ceq "$actualHash  $expectedArchive"
  ) "Windows release checksum does not contain the required hash and filename."

  $smokeRoot = Join-Path ([System.IO.Path]::GetTempPath()) (
    "tessivum 发布 smoke " + [Guid]::NewGuid().ToString('N')
  )
  [System.IO.Directory]::CreateDirectory($smokeRoot) | Out-Null
  $script:ArchiveRoot = Expand-ValidatedReleaseArchive -Archive $archive -Destination (Join-Path $smokeRoot 'archive') -ExpectedRoot ([System.IO.Path]::GetFileNameWithoutExtension($expectedArchive))
  $script:Executable = Join-Path $script:ArchiveRoot 'libexec\tessivum.exe'
  $launcher = Join-Path $script:ArchiveRoot 'bin\tessivum.cmd'
  $script:Launcher = $launcher
  $alias = Join-Path $script:ArchiveRoot 'bin\tsv.cmd'
  $script:MarketFilename = "tessivum-market-$Version.tgz"
  $requiredFiles = @(
    $launcher,
    $alias,
    $script:Executable,
    (Join-Path $script:ArchiveRoot 'share\tessivum\compat-host\src\index.ts'),
    (Join-Path $script:ArchiveRoot 'share\tessivum\host-modules\INVENTORY.json'),
    (Join-Path $script:ArchiveRoot 'share\tessivum\vendor\cordis\lib\index.js'),
    (Join-Path $script:ArchiveRoot 'share\tessivum\vendor\cosmokit\lib\index.js'),
    (Join-Path $script:ArchiveRoot 'share\tessivum\vendor\loader\lib\index.js'),
    (Join-Path $script:ArchiveRoot "share\tessivum\plugins\$script:MarketFilename"),
    (Join-Path $script:ArchiveRoot "share\tessivum\plugins\$script:MarketFilename.sha256"),
    (Join-Path $script:ArchiveRoot "share\tessivum\plugins\$script:MarketFilename.source.json")
  )
  foreach ($required in $requiredFiles) {
    Assert-Release (Test-Path -LiteralPath $required -PathType Leaf) "Release payload is missing $required."
  }
  foreach ($command in @($launcher, $alias)) {
    $contents = [System.IO.File]::ReadAllText($command, $script:Utf8)
    Assert-Release ($contents.EndsWith("`r`n")) "Launcher does not end with CRLF: $command"
    Assert-Release ($contents -notmatch '(?<!\r)\n') "Launcher has a non-CRLF line ending: $command"
    foreach ($marker in @(
      '%~dp0..',
      'TESSIVUM_COMPAT_HOST=%TESSIVUM_ROOT%\share\tessivum\compat-host\src\index.ts',
      'TESSIVUM_HOST_MODULE_ROOT=%TESSIVUM_ROOT%\share\tessivum\host-modules',
      "TESSIVUM_MARKET_TARBALL=%TESSIVUM_ROOT%\share\tessivum\plugins\$script:MarketFilename",
      "TESSIVUM_MARKET_SHA256_FILE=%TESSIVUM_ROOT%\share\tessivum\plugins\$script:MarketFilename.sha256",
      "TESSIVUM_MARKET_SOURCE_FILE=%TESSIVUM_ROOT%\share\tessivum\plugins\$script:MarketFilename.source.json",
      'CORDIS_VENDOR_ROOT=%TESSIVUM_ROOT%\share\tessivum\vendor',
      '"%TESSIVUM_ROOT%\libexec\tessivum.exe" %*'
    )) {
      Assert-Release ($contents.Contains($marker)) "Launcher does not default ${marker}: $command"
    }
    $versionOutput = @(Invoke-PackagedLauncher -Launcher $command -Arguments @('--version') -LogName "$(Split-Path -Leaf $command).version.log")
    Assert-Release (
      $versionOutput.Count -eq 1 -and ([string] $versionOutput[0]).Trim() -ceq "tessivum $Version"
    ) "Launcher version smoke failed for $command."
  }

  Push-Location $script:RepositoryRoot
  $locationPushed = $true
  $replay = Join-Path $script:RepositoryRoot 'fixtures\headless\recorded-replay.jsonl'
  Assert-Release (Test-Path -LiteralPath $replay -PathType Leaf) "Missing packaged headless replay fixture: $replay"
  $headlessState = Join-Path $smokeRoot 'headless state'
  $headlessTask = 'prove the CLI tool round trip'
  $headlessOutput = @(Invoke-PackagedLauncher -Launcher $launcher -Arguments @(
    '--session', 'windows-release-smoke',
    '--data-dir', $headlessState,
    '--replay', $replay,
    '--trusted-bash',
    $headlessTask
  ) -LogName 'headless-first.log')
  Assert-Release (
    $headlessOutput.Count -eq 1 -and ([string] $headlessOutput[0]).Trim() -ceq 'CLI tool round trip complete: CLI_TOOL_ROUND_TRIP'
  ) 'Headless PowerShell smoke produced unexpected output.'
  $sessionFiles = @(Get-ChildItem -LiteralPath $headlessState -Recurse -File -Filter 'session-*.jsonl')
  Assert-Release ($sessionFiles.Count -eq 1 -and $sessionFiles[0].Length -gt 0) 'Headless PowerShell smoke did not persist one session.'
  $sessionPath = $sessionFiles[0].FullName
  Assert-CliSmokeToolRoundTrip -SessionPath $sessionPath -ExpectedResults 1 -Phase 'first run'
  $beforeResume = [System.IO.File]::ReadAllBytes($sessionPath)
  $resumeOutput = @(Invoke-PackagedLauncher -Launcher $launcher -Arguments @(
    '--session', 'windows-release-smoke',
    '--data-dir', $headlessState,
    '--replay', $replay,
    '--trusted-bash',
    '--resume',
    $headlessTask
  ) -LogName 'headless-resume.log')
  Assert-Release (
    $resumeOutput.Count -eq 1 -and ([string] $resumeOutput[0]).Trim() -ceq 'CLI tool round trip complete: CLI_TOOL_ROUND_TRIP'
  ) 'Resumed headless PowerShell smoke produced unexpected output.'
  $sessionFilesAfter = @(Get-ChildItem -LiteralPath $headlessState -Recurse -File -Filter 'session-*.jsonl')
  Assert-Release (
    $sessionFilesAfter.Count -eq 1 -and $sessionFilesAfter[0].FullName -eq $sessionPath
  ) 'Headless PowerShell resume replaced the session file.'
  $afterResume = [System.IO.File]::ReadAllBytes($sessionPath)
  Assert-Release ($afterResume.Length -gt $beforeResume.Length) 'Headless PowerShell resume did not append session history.'
  for ($index = 0; $index -lt $beforeResume.Length; $index += 1) {
    Assert-Release (
      $beforeResume[$index] -eq $afterResume[$index]
    ) "Headless PowerShell resume changed session history at byte $index."
  }
  Assert-CliSmokeToolRoundTrip -SessionPath $sessionPath -ExpectedResults 2 -Phase 'resume'

  $bunCommand = Get-Command bun.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1
  $bun = [string] $bunCommand.Source
  Assert-Release (
    -not [string]::IsNullOrWhiteSpace($bun) -and
    (Test-Path -LiteralPath $bun -PathType Leaf)
  ) "PTC smoke did not resolve bun.exe to a file: $bun"
  $bunDirectory = Split-Path -Parent $bun
  $ptcDirectories = @($bunDirectory, [Environment]::SystemDirectory, $env:WINDIR)
  Assert-Release (
    Test-Path -LiteralPath (Join-Path $bunDirectory 'bun.exe') -PathType Leaf
  ) "PTC smoke Bun directory does not contain bun.exe: $bunDirectory"
  $ptcPath = $ptcDirectories -join ';'
  foreach ($directory in @($ptcDirectories | Where-Object { -not [string]::IsNullOrEmpty($_) })) {
    Assert-Release (
      -not (Test-Path -LiteralPath (Join-Path $directory 'node.exe') -PathType Leaf)
    ) "PTC smoke PATH unexpectedly exposes node.exe in $directory."
  }
  $ptcReplay = Join-Path $smokeRoot 'ptc-runtime-replay.jsonl'
  Write-PtcRuntimeReplay -Path $ptcReplay
  $ptcPort = Get-AvailableLoopbackPort
  $ptcState = Join-Path $smokeRoot 'ptc state'
  # Provision the package manager-dependent first-party market before removing
  # Node from PATH. The worker execution below must not use Node or pnpm.
  $ptcSetup = Start-ReleaseServer -Name 'ptc-setup' -DataDirectory $ptcState -Port $ptcPort
  [void] (Wait-ForWeb -Port $ptcPort -Handle $ptcSetup)
  Stop-ReleaseServer -Handle $ptcSetup -RequireHostShutdown
  $ptc = Start-ReleaseServer -Name 'ptc' -DataDirectory $ptcState -Port $ptcPort -EnvironmentOverrides @{
    PATH = $ptcPath
    TESSIVUM_REPLAY_FILE = $ptcReplay
  }
  [void] (Wait-ForWeb -Port $ptcPort -Handle $ptc)
  $created = Invoke-ClientRequest -Port $ptcPort -Method 'session.create' -RpcId 'release-ptc-session' -Payload @{ sessionId = 'release-ptc-session' }
  Assert-Release (
    $created.result.ok -eq $true -and $created.result.value.sessionId -eq 'release-ptc-session'
  ) 'PTC smoke could not create a session.'
  $selected = Invoke-ClientRequest -Port $ptcPort -Method 'agentPreset.select' -RpcId 'release-ptc-select' -Payload @{
    sessionId = 'release-ptc-session'
    agentPreset = 'ptc'
  }
  Assert-Release (
    $selected.result.ok -eq $true -and $selected.result.value.agentPreset -eq 'ptc'
  ) 'PTC smoke could not select the PTC preset.'
  $ptcPrompt = Invoke-ClientRequest -Port $ptcPort -Method 'session.prompt' -RpcId 'release-ptc-prompt' -Payload @{
    sessionId = 'release-ptc-session'
    mode = 'queue'
    content = @(
      @{ type = 'text'; text = 'Run the deterministic PTC runtime smoke.' }
    )
  }
  Assert-Release (
    $ptcPrompt.result.ok -eq $true -and $ptcPrompt.result.value.accepted -eq $true
  ) 'PTC smoke did not accept the runtime prompt.'
  $ptcEvents = @(Wait-ForReleaseSessionTurn -Port $ptcPort -Handle $ptc -SessionId 'release-ptc-session')
  Assert-PtcRuntimeRoundTrip -Events $ptcEvents
  $listed = Invoke-ClientRequest -Port $ptcPort -Method 'agentPreset.list' -RpcId 'release-native-roster' -Payload @{}
  $modeIds = @($listed.result.value.presets | ForEach-Object { [string] $_.id })
  Assert-Release (
    $listed.result.ok -eq $true -and ($modeIds -join ',') -ceq 'standard,ptc,minimal,composition'
  ) 'PTC smoke received an unexpected native preset roster.'
  foreach ($mode in @('standard', 'ptc', 'minimal', 'composition')) {
    $read = Invoke-ClientRequest -Port $ptcPort -Method 'agentPreset.read' -RpcId "release-mode-$mode" -Payload @{ agentPreset = $mode }
    $content = [string] $read.result.value.content
    Assert-Release (
      $read.result.ok -eq $true -and
      $read.result.value.agentPreset -eq $mode -and
      $content -match '(?m)^schema = 1$' -and
      $content -match ('(?m)^id = "' + [regex]::Escape($mode) + '"$')
    ) "PTC smoke could not read native mode $mode."
  }
  $copied = Invoke-ClientRequest -Port $ptcPort -Method 'agentPreset.copy' -RpcId 'release-user-mode-copy' -Payload @{
    from = 'standard'
    agentPreset = 'release-native-mode'
    name = 'Release Native Mode'
  }
  Assert-Release (
    $copied.result.ok -eq $true -and $copied.result.value.agentPreset -eq 'release-native-mode'
  ) 'PTC smoke could not copy a native mode.'
  $copiedMode = Join-Path $ptcState 'modes\release-native-mode\mode.toml'
  Assert-Release (Test-Path -LiteralPath $copiedMode -PathType Leaf) 'PTC smoke did not persist the copied mode.'
  $copiedModeText = [System.IO.File]::ReadAllText($copiedMode, $script:Utf8)
  Assert-Release (
    $copiedModeText -match '(?m)^schema = 1$' -and $copiedModeText -match '(?m)^id = "release-native-mode"$'
  ) 'PTC smoke copied an invalid mode document.'
  Assert-Release (
    -not (Test-Path -LiteralPath (Join-Path $ptcState '.agent-presets'))
  ) 'PTC smoke recreated legacy preset assets.'
  Stop-ReleaseServer -Handle $ptc -RequireHostShutdown

  $dreamState = Join-Path $smokeRoot 'dream skin state'
  [void] (Invoke-PackagedLauncher -Launcher $launcher -Arguments @(
    '--data-dir', $dreamState,
    'plugin', 'add', 'dsh-dream-skin@8.30.1'
  ) -LogName 'market-dream-skin-add.log')
  $dreamManifestPath = Join-Path $dreamState 'plugins\package.json'
  Assert-Release (Test-Path -LiteralPath $dreamManifestPath -PathType Leaf) 'Market smoke did not create the dream-skin profile manifest.'
  $dreamManifest = [System.IO.File]::ReadAllText($dreamManifestPath, $script:Utf8) | ConvertFrom-Json
  Assert-Release (
    $dreamManifest.dependencies.'dsh-dream-skin' -eq '8.30.1' -and
    @($dreamManifest.dsh.profile.bundles) -contains 'dsh-dream-skin'
  ) 'Market smoke did not install the expected dream-skin bundle.'
  $dreamPort = Get-AvailableLoopbackPort
  $dream = Start-ReleaseServer -Name 'dream-skin' -DataDirectory $dreamState -Port $dreamPort -EnvironmentOverrides @{ DSH_HOME = $dreamState }
  $dreamIndex = Wait-ForWeb -Port $dreamPort -Handle $dream
  $bootMatch = [regex]::Match(
    $dreamIndex,
    'window\.__DSH_BOOT__=(\{.*?\});</script>',
    [System.Text.RegularExpressions.RegexOptions]::Singleline
  )
  Assert-Release ($bootMatch.Success) 'Market smoke did not serve a Browser boot graph.'
  $boot = $bootMatch.Groups[1].Value | ConvertFrom-Json
  $dreamEntries = @($boot.entries | Where-Object { $_.id -eq 'dsh-dream-skin' })
  Assert-Release ($dreamEntries.Count -eq 1) 'Market smoke boot graph lacks dsh-dream-skin.'
  $dreamBundleUrl = [string] $dreamEntries[0].url
  Assert-Release (
    $dreamBundleUrl.StartsWith('/plugins/dsh-dream-skin/client.js?rev=', [System.StringComparison]::Ordinal)
  ) 'Market smoke gave dsh-dream-skin an invalid Browser bundle URL.'
  $dreamBundle = (Invoke-WebRequest -Uri "http://127.0.0.1:$dreamPort$dreamBundleUrl" -TimeoutSec 2).Content
  Assert-Release ($dreamBundle.Contains('dsh-dream-skin')) 'Market smoke did not serve the dream-skin Browser bundle.'
  $dreamHeaders = @{ Origin = "http://127.0.0.1:$dreamPort"; 'Sec-Fetch-Site' = 'same-origin' }
  $dreamSet = Invoke-ReleaseJson -Uri "http://127.0.0.1:$dreamPort/dream-skin/api" -Headers $dreamHeaders -Body @{
    method = 'set'
    patch = @{ skin = 'abyss' }
  }
  Assert-Release ($dreamSet.ok -eq $true) 'Market smoke could not persist dream-skin state.'
  $dreamGet = Invoke-ReleaseJson -Uri "http://127.0.0.1:$dreamPort/dream-skin/api" -Headers $dreamHeaders -Body @{ method = 'get' }
  Assert-Release (
    $dreamGet.ok -eq $true -and $dreamGet.value.skin -eq 'abyss'
  ) 'Market smoke could not read dream-skin state.'
  [void] (Invoke-ReleaseRestart -Handle $dream)
  $dreamGetAfterRestart = Invoke-ReleaseJson -Uri "http://127.0.0.1:$dreamPort/dream-skin/api" -Headers $dreamHeaders -Body @{ method = 'get' }
  Assert-Release (
    $dreamGetAfterRestart.ok -eq $true -and $dreamGetAfterRestart.value.skin -eq 'abyss'
  ) 'Market smoke lost dream-skin state after a real product restart.'
  Stop-ReleaseServer -Handle $dream -RequireHostShutdown

  $legacyState = Join-Path $smokeRoot 'legacy state'
  $timerBundle = (Resolve-Path -LiteralPath (Join-Path $script:RepositoryRoot 'fixtures\release\timer-bundle') -ErrorAction Stop).Path
  $timerSpecifier = [System.Uri]::new($timerBundle).AbsoluteUri
  [void] (Invoke-PackagedLauncher -Launcher $launcher -Arguments @(
    '--data-dir', $legacyState,
    'plugin', 'add', $timerSpecifier
  ) -LogName 'legacy-timer-add.log')
  $legacyPort = Get-AvailableLoopbackPort
  $legacy = Start-ReleaseServer -Name 'legacy-timer' -DataDirectory $legacyState -Port $legacyPort
  [void] (Wait-ForWeb -Port $legacyPort -Handle $legacy)
  $inventory = Invoke-ClientRequest -Port $legacyPort -Method 'pluginInventory/list' -RpcId 'release-plugin-smoke' -Payload @{ args = @() }
  $timerEntries = @($inventory.result.value.entries | Where-Object {
    $_.moduleName -eq '@tessivum/release-timer-bundle' -and
    $_.enabled -eq $true -and
    $_.fiberPhase -eq 'active'
  })
  Assert-Release (
    $inventory.result.ok -eq $true -and $timerEntries.Count -eq 1
  ) 'Legacy smoke did not activate the packaged timer bundle.'
  Stop-ReleaseServer -Handle $legacy -RequireHostShutdown

  Write-ReleaseLines -Name 'summary.txt' -Lines @(
    "archive=$archive",
    "version=$Version",
    "archive-root=$script:ArchiveRoot",
    'version-launchers=passed',
    'headless-powershell=passed',
    'ptc-without-node=passed',
    'market-browser-restart=passed',
    'legacy-shutdown=passed'
  )
} finally {
  foreach ($server in $script:ActiveServers.ToArray()) {
    try {
      Force-StopReleaseServer -Handle $server
    } catch {
      Write-ReleaseLines -Name "cleanup-$($server.Name).log" -Lines @($_.Exception.Message)
    }
  }
  if ($null -ne $smokeRoot -and (Test-Path -LiteralPath $smokeRoot)) {
    Remove-Item -LiteralPath $smokeRoot -Recurse -Force -ErrorAction SilentlyContinue
  }
  if ($locationPushed) {
    Pop-Location
  }
}
