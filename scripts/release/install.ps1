<#
Install the loadout CLI on Windows from a GitHub release, then optionally run `loadout init`.

  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/dan-myles/loadout/main/scripts/release/install.ps1)))
  & ([scriptblock]::Create((irm .../install.ps1))) -LoadoutUrl https://github.com/you/your-loadout.git -InitArgs '--name','desktop'
  powershell -ExecutionPolicy Bypass -File install.ps1 -Version v1.2.3

-Version vX.Y.Z  install that release instead of the latest one
-BaseUrl URL     fetch the assets, and the package managers' installers, from URL instead of
                 their usual hosts: https, or http on localhost
-LoadoutUrl URL  afterwards, run `loadout init URL` with each -InitArgs element as one argument
-NoPackageManagers  never install winget, Scoop, or Chocolatey
Before installing, it sets up the package managers a loadout's programs install with: winget when
App Installer is present but unregistered, Scoop for this user, and Chocolatey when run as an
administrator. git comes from Scoop when it is missing.
An existing loadout.exe is replaced only if this installer put it there. docs/releasing.md has details.
#>
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$BaseUrl = '',
    [string]$LoadoutUrl = '',
    [string[]]$InitArgs = @(),
    [switch]$NoPackageManagers
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Repo = 'dan-myles/loadout'
$RecordKeys = @('schema', 'installer', 'repository', 'source', 'target', 'version', 'archive_sha256', 'binary', 'binary_sha256')
$BackgroundKeys = @('background', 'background_sha256')
# The release zip ships both: the console CLI and the windowless launcher its scheduled task starts.
$Binaries = @('loadout.exe', 'loadout-background.exe')
$MaxRedirects = 5
$LoopbackHosts = @('127.0.0.1', 'localhost', '[::1]')

function Say([string]$Message) { [Console]::Error.WriteLine("loadout-install: $Message") }

function Get-NativeArch {
    # PROCESSOR_ARCHITECTURE describes this process, which is AMD64 for x64 PowerShell emulated on
    # ARM64 Windows; the machine-wide value in the registry is the native one.
    $key = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
    $native = (Get-ItemProperty -LiteralPath $key -Name PROCESSOR_ARCHITECTURE).PROCESSOR_ARCHITECTURE
    switch ($native) {
        'AMD64' { return 'x86_64' }
        'ARM64' { return 'aarch64' }
        default { throw "unsupported CPU architecture ${native}: releases exist for x64 and ARM64" }
    }
}

function Test-Reparse([string]$Path) {
    (Test-Path -LiteralPath $Path) -and
        (((Get-Item -LiteralPath $Path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Assert-DownloadUri([Uri]$Uri, [bool]$LoopbackHttp) {
    if ($Uri.UserInfo) { throw "refusing ${Uri}: URLs with credentials are not allowed" }
    if ($Uri.Scheme -ceq 'https') { return }
    if ($LoopbackHttp -and $Uri.Scheme -ceq 'http' -and $LoopbackHosts -contains $Uri.Host) { return }
    throw "refusing ${Uri}: every download must use https"
}

function Get-BaseUri([string]$Url) {
    if ($Url -match '[\s\x00-\x1F\x7F?#@\\"'']') {
        throw '-BaseUrl must be a plain URL: no spaces, quotes, credentials, query, or fragment'
    }
    $uri = $null
    if (-not [Uri]::TryCreate($Url, [UriKind]::Absolute, [ref]$uri) -or -not $uri.Host) {
        throw "-BaseUrl $Url is not an absolute URL"
    }
    if ($uri.Scheme -ceq 'http' -and -not ($LoopbackHosts -contains $uri.Host)) {
        throw '-BaseUrl may use http only for 127.0.0.1, localhost, or [::1]'
    }
    Assert-DownloadUri $uri ($uri.Scheme -ceq 'http')
    $uri
}

function New-HttpClient {
    Add-Type -AssemblyName System.Net.Http
    # Windows PowerShell 5.1's .NET Framework may not offer TLS 1.2 by default; this adds it.
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $handler = New-Object Net.Http.HttpClientHandler
    $handler.AllowAutoRedirect = $false
    $client = New-Object Net.Http.HttpClient($handler)
    $client.Timeout = [TimeSpan]::FromMinutes(10)
    $client
}

function Save-Url($Client, [string]$Url, [string]$OutFile, [bool]$LoopbackHttp) {
    $uri = New-Object Uri($Url)
    $allowed = if ($LoopbackHttp) { 0 } else { $MaxRedirects }
    for ($hop = 0; ; $hop++) {
        Assert-DownloadUri $uri $LoopbackHttp
        $response = $null
        try {
            try {
                $response = $Client.GetAsync($uri, [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            } catch {
                throw "download failed: $uri ($($_.Exception.Message))"
            }
            $code = [int]$response.StatusCode
            if ($code -ge 300 -and $code -lt 400) {
                if ($hop -ge $allowed) { throw "download failed: $uri redirects more than $allowed times" }
                $next = $response.Headers.Location
                if ($null -eq $next) { throw "download failed: $uri redirects without a location" }
                if (-not $next.IsAbsoluteUri) { $next = New-Object Uri($uri, $next) }
                $uri = $next
                continue
            }
            if (-not $response.IsSuccessStatusCode) { throw "download failed: $uri (HTTP $code)" }
            $source = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
            $file = [IO.File]::Open($OutFile, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try { $source.CopyTo($file) } finally { $file.Dispose(); $source.Dispose() }
            return
        } finally {
            if ($response) { $response.Dispose() }
        }
    }
}

# Bytes, not decoded text: ReadAllText would silently strip a UTF-8 or UTF-16 byte order mark.
function Read-Sidecar([string]$Path, [string]$Asset) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    foreach ($byte in $bytes) {
        if ($byte -ne 10 -and ($byte -lt 32 -or $byte -gt 126)) {
            throw "$Asset.sha256 is not exactly one '<sha256>  $Asset' line"
        }
    }
    $text = [Text.Encoding]::ASCII.GetString($bytes)
    $match = [regex]::Match($text, '\A([0-9a-f]{64})  (\S+)\n\z')
    if (-not $match.Success -or $match.Groups[2].Value -cne $Asset) {
        throw "$Asset.sha256 is not exactly one '<sha256>  $Asset' line"
    }
    $match.Groups[1].Value
}

# Exactly one regular loadout-<target>/<name> for each of $Binaries, besides README.md and LICENSE*,
# and no entry outside loadout-<target>/ or nested in it. Only the binaries are written, to $OutDir.
function Export-ZipBinaries([string]$Zip, [string]$Target, [string]$OutDir) {
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $prefix = "loadout-$Target/"
    $archive = [IO.Compression.ZipFile]::OpenRead($Zip)
    try {
        $found = @{}
        foreach ($entry in $archive.Entries) {
            $name = $entry.FullName
            if ($name -ceq $prefix) { continue }
            $leaf = if ($name.StartsWith($prefix, [StringComparison]::Ordinal)) { $name.Substring($prefix.Length) } else { '' }
            if ($leaf -cnotmatch '\A[A-Za-z0-9._-]+\z' -or $leaf -eq '.' -or $leaf -eq '..') {
                throw "the archive has an unexpected entry '$name'"
            }
            if ($Binaries -ccontains $leaf) {
                if ($found.ContainsKey($leaf)) { throw "the archive has more than one $name" }
                $type = ($entry.ExternalAttributes -shr 16) -band 0xF000
                if ($type -ne 0 -and $type -ne 0x8000) { throw "$name in the archive is not a regular file" }
                $found[$leaf] = $entry
            } elseif ($leaf -cne 'README.md' -and -not $leaf.StartsWith('LICENSE', [StringComparison]::Ordinal)) {
                throw "the archive has an entry that is not part of a loadout release: '$name'"
            }
        }
        $paths = @{}
        foreach ($binary in $Binaries) {
            if (-not $found.ContainsKey($binary)) { throw "the archive has no $prefix$binary" }
            $out = Join-Path $OutDir $binary
            $source = $found[$binary].Open()
            $file = [IO.File]::Open($out, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try { $source.CopyTo($file) } finally { $file.Dispose(); $source.Dispose() }
            $paths[$binary] = $out
        }
        $paths
    } finally {
        $archive.Dispose()
    }
}

# Install-record strings never hold control characters (Windows paths can't), so only backslash and
# double quote need escaping; unpaired surrogates fail the strict UTF-8 encoder before anything changes.
function ConvertTo-TomlString([string]$Value) {
    if ($Value -match '[\x00-\x1F\x7F]') { throw "'$Value' contains a control character, which the install record can't hold" }
    '"' + $Value.Replace('\', '\\').Replace('"', '\"') + '"'
}

# Data only: the header comment, `schema = 1` or `schema = 2`, and one basic string per known key of
# that schema (schema 2 adds the background launcher). Values stay encoded.
function Read-InstallRecord([string]$Path) {
    $text = (New-Object Text.UTF8Encoding($false, $true)).GetString([IO.File]::ReadAllBytes($Path))
    if (-not $text.EndsWith("`n")) { throw 'it does not end with a newline' }
    $values = @{}
    foreach ($line in $text.Substring(0, $text.Length - 1).Split([char]10)) {
        if ($line -cmatch '\A#[^\x00-\x1F\x7F]*\z') { continue }
        if ($line -ceq 'schema = 1' -or $line -ceq 'schema = 2') {
            $key = 'schema'; $value = $line.Substring(9)
        } else {
            $match = [regex]::Match($line, '\A([a-z0-9_]+) = ("(?:[^"\\\x00-\x1F\x7F]|\\["\\])*")\z')
            if (-not $match.Success) { throw "line '$line' is not part of the install record format" }
            $key = $match.Groups[1].Value; $value = $match.Groups[2].Value
            if ($key -ceq 'schema' -or -not (($RecordKeys + $BackgroundKeys) -ccontains $key)) { throw "unknown key $key" }
        }
        if ($values.ContainsKey($key)) { throw "$key appears twice" }
        $values[$key] = $value
    }
    foreach ($key in $RecordKeys) {
        if (-not $values.ContainsKey($key)) { throw "$key is missing" }
    }
    foreach ($key in $BackgroundKeys) {
        if (($values['schema'] -ceq '2') -ne $values.ContainsKey($key)) {
            throw "$key does not fit a schema $($values['schema']) record"
        }
    }
    $values
}

function Get-RecordMismatch([string]$Record, [string]$Dest, [string]$Background, [string]$Target) {
    if (-not (Test-Path -LiteralPath $Record -PathType Leaf)) { return "there is no install record $Record" }
    try {
        $values = Read-InstallRecord $Record
    } catch {
        return "$Record is not a valid install record: $($_.Exception.Message)"
    }
    if ($values['installer'] -cne '"install.ps1"') { return "$Record was written by another installer" }
    if ($values['target'] -cne "`"$Target`"") { return "$Record is for another target than $Target" }
    if (-not [string]::Equals($values['binary'], (ConvertTo-TomlString $Dest), [StringComparison]::OrdinalIgnoreCase)) {
        return "$Record describes another binary path"
    }
    if ($values['binary_sha256'] -cne "`"$(Get-Sha256 $Dest)`"") { return "$Dest changed after it was installed" }
    if ($values['schema'] -ceq '1') {
        # A schema-1 install had no launcher; one there now came from somewhere else.
        if (Test-Path -LiteralPath $Background) { return "$Background is not in the schema 1 record $Record" }
        return $null
    }
    if (-not [string]::Equals($values['background'], (ConvertTo-TomlString $Background), [StringComparison]::OrdinalIgnoreCase)) {
        return "$Record describes another background launcher"
    }
    if (-not (Test-Path -LiteralPath $Background -PathType Leaf)) { return "$Background is missing" }
    if ($values['background_sha256'] -cne "`"$(Get-Sha256 $Background)`"") { return "$Background changed after it was installed" }
    $null
}

function Assert-Destination([string]$Dest, [string]$Background, [string]$Record, [string]$Target) {
    foreach ($path in @($Dest, $Background, $Record)) {
        if ((Test-Path -LiteralPath $path -PathType Container) -or (Test-Reparse $path)) {
            throw "$path is a link or a folder, so another installer may own it. It was left alone"
        }
    }
    if (Test-Path -LiteralPath $Dest) {
        $why = Get-RecordMismatch $Record $Dest $Background $Target
        if ($why) {
            throw "$Dest was not installed by this installer ($why), so it was left alone. To install here, remove $Dest, $Background and $Record yourself first"
        }
    } elseif (Test-Path -LiteralPath $Background) {
        throw "$Background is there without loadout.exe, so another installer may own it. It was left alone"
    }
}

# install.sh, install.ps1 and loadout self-update each hold this lock from their last check of the
# installed files until the record is in place, so none overwrites what another just installed. On
# Windows it is the file opened unshared, as loadout opens it; the file itself stays.
function Open-InstallLock([string]$Dir) {
    $path = Join-Path $Dir '.loadout.install.lock'
    if ((Test-Path -LiteralPath $path -PathType Container) -or (Test-Reparse $path)) {
        throw "$path is a link or a folder, so this install can't be locked. It was left alone"
    }
    try {
        [IO.File]::Open($path, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    } catch {
        throw "another install or loadout self-update is changing $Dir right now, so nothing was replaced. Try again when it is done ($($_.Exception.Message))"
    }
}

# The loadout URL as loadout itself shows it: the userinfo of a scheme:// URL, which can be a token,
# becomes ***. An scp-like user@host:path keeps its SSH user name.
function Get-ShownUrl([string]$Url) {
    $scheme = $Url.IndexOf('://')
    if ($scheme -lt 0) { return $Url }
    $rest = $Url.Substring($scheme + 3)
    $slash = $rest.IndexOf('/')
    $authority = if ($slash -ge 0) { $rest.Substring(0, $slash) } else { $rest }
    $at = $authority.LastIndexOf('@')
    if ($at -lt 0) { return $Url }
    $Url.Substring(0, $scheme + 3) + '***' + $rest.Substring($at)
}

# The sha256 of each path, or 'absent', as one line.
function Get-InstallState([string[]]$Paths) {
    ($Paths | ForEach-Object { if (Test-Path -LiteralPath $_) { Get-Sha256 $_ } else { 'absent' } }) -join ' '
}

function New-StagedFile($Owned, [string]$Directory, [string]$Prefix, [byte[]]$Bytes) {
    $path = Join-Path $Directory ($Prefix + [Guid]::NewGuid().ToString('N'))
    $stream = [IO.File]::Open($path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    $Owned.Add($path)
    try { $stream.Write($Bytes, 0, $Bytes.Length) } finally { $stream.Dispose() }
    $path
}

function Publish-StagedFile($Owned, [string]$Staged, [string]$Destination) {
    if (Test-Path -LiteralPath $Destination) {
        [IO.File]::Replace($Staged, $Destination, [NullString]::Value)
    } else {
        [IO.File]::Move($Staged, $Destination)
    }
    [void]$Owned.Remove($Staged)
}

# Windows can't overwrite a running executable, but it can rename one, so each existing binary is
# first moved aside to `.<name>.old.<guid>`. If a later step fails, every completed move is undone
# in reverse and the record is never touched. The binaries are not replaced as one atomic step.
# Returns the aside paths this run made; the caller deletes them only once the record is in place.
# Each move records its role when it is made, never inferred from path text, because any parent
# folder may contain `.old.` or `.new.`: 'aside' moved an existing binary to its aside name,
# 'staged' moved this run's staging file to the destination.
function Publish-Binaries($Owned, [object[]]$Pairs) {
    $moves = New-Object Collections.Generic.List[object]
    try {
        foreach ($pair in $Pairs) {
            $staged = $pair[0]
            $dest = $pair[1]
            if (Test-Path -LiteralPath $dest) {
                $aside = Join-Path (Split-Path -Parent $dest) (".$(Split-Path -Leaf $dest).old." + [Guid]::NewGuid().ToString('N'))
                [IO.File]::Move($dest, $aside)
                $moves.Add([pscustomobject]@{ From = $dest; To = $aside; Role = 'aside' })
            }
            [IO.File]::Move($staged, $dest)
            [void]$Owned.Remove($staged)
            $moves.Add([pscustomobject]@{ From = $staged; To = $dest; Role = 'staged' })
        }
    } catch {
        $failure = $_.Exception.Message
        $restored = $true
        for ($index = $moves.Count - 1; $index -ge 0; $index--) {
            $move = $moves[$index]
            try {
                [IO.File]::Move($move.To, $move.From)
                # A staging file moved back is this run's to clean up again. A binary moved back
                # from its aside is the installed one, and never is.
                if ($move.Role -ceq 'staged') { $Owned.Add($move.From) }
            } catch {
                $restored = $false
            }
        }
        if ($restored) {
            throw "could not put the new binaries in place, so the previous ones are back ($failure)"
        }
        throw "could not put the new binaries in place, and the previous ones could not all be restored; reinstall with install.ps1 ($failure)"
    }
    foreach ($move in $moves) {
        if ($move.Role -ceq 'aside') { $move.To }
    }
}

# The binaries, then the record, then this run's own asides: exactly the paths Publish-Binaries
# made. If the record can't be written, those old binaries stay for recovery. An aside that is
# still running stays too, and no other file is ever deleted.
function Publish-Install($Owned, [object[]]$Pairs, [string]$StagedRecord, [string]$Record) {
    $asides = @(Publish-Binaries $Owned $Pairs)
    try {
        Publish-StagedFile $Owned $StagedRecord $Record
    } catch {
        $kept = if ($asides.Count) { "; the previous binaries are kept as $($asides -join ', ')" } else { '' }
        throw "the new binaries are in place but $Record could not be written, so self-update will not replace them$kept ($($_.Exception.Message))"
    }
    foreach ($aside in $asides) { Remove-Item -LiteralPath $aside -Force -ErrorAction SilentlyContinue }
}

# Quotes one argument so CommandLineToArgvW, the MSVC runtime, and Rust's std::env::args all read it
# back unchanged, including empty strings, embedded quotes, and trailing backslashes.
function ConvertTo-NativeArgument([string]$Value) {
    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') { return $Value }
    $backslash = [char]92
    $quote = [char]34
    $quoted = New-Object Text.StringBuilder
    [void]$quoted.Append($quote)
    $backslashes = 0
    foreach ($char in $Value.ToCharArray()) {
        if ($char -eq $backslash) {
            $backslashes++
            continue
        }
        if ($char -eq $quote) {
            [void]$quoted.Append($backslash, 2 * $backslashes + 1)
        } elseif ($backslashes -gt 0) {
            [void]$quoted.Append($backslash, $backslashes)
        }
        [void]$quoted.Append($char)
        $backslashes = 0
    }
    [void]$quoted.Append($backslash, 2 * $backslashes)
    [void]$quoted.Append($quote)
    $quoted.ToString()
}

function Invoke-Native([string]$FilePath, [string[]]$Arguments) {
    $encoded = New-Object Collections.Generic.List[string]
    foreach ($argument in $Arguments) { $encoded.Add((ConvertTo-NativeArgument $argument)) }
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $FilePath
    $info.Arguments = $encoded -join ' '
    $info.UseShellExecute = $false
    $process = [Diagnostics.Process]::Start($info)
    try {
        $process.WaitForExit()
        $process.ExitCode
    } finally {
        $process.Dispose()
    }
}

function Test-Command([string]$Name) {
    [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

# Picks up what the package managers' installers added to the machine and user Path, for the init
# below and the background service it records PATH for.
function Update-SessionPath {
    $env:Path = @(
        [Environment]::GetEnvironmentVariable('Path', 'Machine'),
        [Environment]::GetEnvironmentVariable('Path', 'User')
    ) -join ';'
}

function Invoke-Installer($Client, [string]$Name, [string]$Url, [string]$Tmp, [bool]$LoopbackHttp, [hashtable]$Arguments) {
    $file = Join-Path $Tmp "$Name-install.ps1"
    Say "$Name is not installed, so running its installer from $Url (pass -NoPackageManagers to skip)"
    Save-Url $Client $Url $file $LoopbackHttp
    # A script block, not the file, so the execution policy's rule for downloaded files doesn't apply.
    & ([scriptblock]::Create([IO.File]::ReadAllText($file))) @Arguments
}

# winget, then Scoop, then Chocolatey: the order loadout picks managers in on Windows. Only winget
# failing to register is a warning; any other installer failing stops the install.
function Install-PackageManagers($Client, [string]$Tmp, [bool]$LoopbackHttp) {
    $base = if ($BaseUrl) { $BaseUrl.TrimEnd('/') } else { '' }
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)

    if (-not (Test-Command winget)) {
        Say 'winget is not on PATH, so registering App Installer, which provides it'
        try {
            Add-AppxPackage -RegisterByFamilyName -MainPackage Microsoft.DesktopAppInstaller_8wekyb3d8bbwe
        } catch {
            Say "warning: could not register App Installer ($($_.Exception.Message)). Install App Installer from the Microsoft Store to get winget"
        }
    }

    if (-not (Test-Command scoop)) {
        # Scoop's installer checks the effective policy; Bypass for this process alone satisfies it.
        Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass -Force
        $url = if ($base) { "$base/scoop-install.ps1" } else { 'https://get.scoop.sh' }
        $arguments = if ($admin) { @{ RunAsAdmin = $true } } else { @{} }
        Invoke-Installer $Client 'Scoop' $url $Tmp $LoopbackHttp $arguments
        Update-SessionPath
        if (-not (Test-Command scoop)) { throw "Scoop's installer finished, but scoop is not on PATH" }
    }

    if (-not (Test-Command choco)) {
        if ($admin) {
            $url = if ($base) { "$base/chocolatey-install.ps1" } else { 'https://community.chocolatey.org/install.ps1' }
            Invoke-Installer $Client 'Chocolatey' $url $Tmp $LoopbackHttp @{}
            Update-SessionPath
            if (-not (Test-Command choco)) { throw "Chocolatey's installer finished, but choco is not on PATH" }
        } else {
            Say 'Chocolatey needs an administrator to install, so it was skipped. Run this installer as an administrator to add it'
        }
    }

    if (-not (Get-Command git -CommandType Application -ErrorAction SilentlyContinue)) {
        Say 'git is not on PATH, so installing it with scoop'
        $ErrorActionPreference = 'Continue'
        & scoop install git
        $scoopExit = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($scoopExit -ne 0) { throw "scoop install git failed with exit code $scoopExit" }
        Update-SessionPath
    }
}

function Install-Loadout {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        throw 'install.ps1 is for Windows; use install.sh on macOS and Linux'
    }
    if ($Version -ne 'latest' -and $Version -cnotmatch '\Av\d+\.\d+\.\d+[0-9A-Za-z.+-]*\z') {
        throw "-Version must be a release tag like v1.2.3, not '$Version'"
    }
    $loopbackHttp = $false
    if ($BaseUrl) {
        $base = Get-BaseUri $BaseUrl
        $loopbackHttp = $base.Scheme -ceq 'http'
    }
    if ($LoadoutUrl.StartsWith('-')) { throw 'the loadout URL must not start with -' }
    foreach ($argument in @($LoadoutUrl) + $InitArgs) {
        if ("$argument".Contains([string][char]0)) { throw 'init arguments must not contain a NUL character' }
    }

    $arch = Get-NativeArch
    $target = "$arch-pc-windows-msvc"

    if (-not $env:LOCALAPPDATA -or -not [IO.Path]::IsPathRooted($env:LOCALAPPDATA)) {
        throw 'LOCALAPPDATA is not set to an absolute path'
    }
    $dir = [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'Programs\loadout'))
    $dest = Join-Path $dir 'loadout.exe'
    $background = Join-Path $dir 'loadout-background.exe'
    $record = Join-Path $dir 'loadout.install.toml'
    [void](ConvertTo-TomlString $dest)
    [void](ConvertTo-TomlString $background)
    Assert-Destination $dest $background $record $target
    $seen = Get-InstallState @($dest, $background, $record)

    $asset = "loadout-$target.zip"
    if ($BaseUrl) {
        $url = "$($BaseUrl.TrimEnd('/'))/$asset"
    } elseif ($Version -eq 'latest') {
        $url = "https://github.com/$Repo/releases/latest/download/$asset"
    } else {
        $url = "https://github.com/$Repo/releases/download/$Version/$asset"
    }
    [void](ConvertTo-TomlString $url)

    $owned = New-Object Collections.Generic.List[string]
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('loadout-install-' + [Guid]::NewGuid().ToString('N'))
    $client = $null
    $lock = $null
    $tmpOwned = $false
    try {
        [void](New-Item -ItemType Directory -Path $tmp)
        $tmpOwned = $true
        $client = New-HttpClient
        if (-not $NoPackageManagers) { Install-PackageManagers $client $tmp $loopbackHttp }
        if (-not (Get-Command git -CommandType Application -ErrorAction SilentlyContinue)) {
            throw 'git is required but is not on PATH. Install it with winget install --id Git.Git -e --source winget, then open a new terminal and run this installer again'
        }
        $zip = Join-Path $tmp $asset
        Say "downloading $url"
        Save-Url $client $url $zip $loopbackHttp
        Save-Url $client "$url.sha256" "$zip.sha256" $loopbackHttp

        $want = Read-Sidecar "$zip.sha256" $asset
        $got = Get-Sha256 $zip
        if ($got -cne $want) { throw "checksum mismatch for ${asset}: expected $want, got $got" }
        Say "verified sha256 $got"

        $unpacked = Join-Path $tmp 'unpacked'
        [void](New-Item -ItemType Directory -Path $unpacked)
        $extracted = Export-ZipBinaries $zip $target $unpacked
        $new = $extracted['loadout.exe']
        $newBackground = $extracted['loadout-background.exe']

        # Windows PowerShell 5.1 turns redirected native stderr into errors that 'Stop' would throw.
        $ErrorActionPreference = 'Continue'
        $reported = (& $new --version 2>$null | Out-String).Trim()
        $exitCode = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        $versionMatch = [regex]::Match($reported, '\Aloadout ([0-9A-Za-z.+-]+)(\s|\z)')
        if ($exitCode -ne 0 -or -not $versionMatch.Success) {
            throw "the downloaded loadout.exe does not run on this machine ($target): '$reported'"
        }
        $installedVersion = $versionMatch.Groups[1].Value
        if ($Version -ne 'latest' -and "v$installedVersion" -cne $Version) {
            throw "release $Version contains loadout $installedVersion"
        }
        $ErrorActionPreference = 'Continue'
        $launcherReported = (& $newBackground --version 2>$null | Out-String).TrimEnd()
        $launcherExit = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($launcherExit -ne 0 -or $launcherReported -cne "loadout-background $installedVersion") {
            throw "the downloaded loadout-background.exe printed '$launcherReported', not 'loadout-background $installedVersion'"
        }

        [void](New-Item -ItemType Directory -Path $dir -Force)
        # Another install may have finished while this one downloaded, so check again under the lock.
        $lock = Open-InstallLock $dir
        Assert-Destination $dest $background $record $target
        if ((Get-InstallState @($dest, $background, $record)) -cne $seen) {
            throw "$dir changed while this install was downloading, so nothing was replaced. Run the installer again"
        }

        $staged = New-StagedFile $owned $dir '.loadout.exe.new.' ([IO.File]::ReadAllBytes($new))
        $stagedBackground = New-StagedFile $owned $dir '.loadout-background.exe.new.' ([IO.File]::ReadAllBytes($newBackground))
        $lines = @(
            '# loadout install record. Written by install.sh or install.ps1 and read by loadout self-update. Do not edit.',
            'schema = 2',
            'installer = "install.ps1"',
            "repository = `"$Repo`"",
            "source = $(ConvertTo-TomlString $url)",
            "target = `"$target`"",
            "version = `"$installedVersion`"",
            "archive_sha256 = `"$want`"",
            "binary = $(ConvertTo-TomlString $dest)",
            "binary_sha256 = `"$(Get-Sha256 $staged)`"",
            "background = $(ConvertTo-TomlString $background)",
            "background_sha256 = `"$(Get-Sha256 $stagedBackground)`""
        )
        $recordBytes = (New-Object Text.UTF8Encoding($false, $true)).GetBytes(($lines -join "`n") + "`n")
        $stagedRecord = New-StagedFile $owned $dir '.loadout.install.toml.new.' $recordBytes

        # The launcher first: until the record names it, a run that starts meanwhile finds a launcher
        # of this release beside whichever loadout.exe is in place.
        $pairs = @(@($stagedBackground, $background), @($staged, $dest))
        Publish-Install $owned $pairs $stagedRecord $record
        Say "installed loadout $installedVersion ($target) to $dest, with $background"

        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if (-not (($userPath -split ';') -contains $dir)) {
            Say "$dir is not on your PATH. To add it for new terminals, run:"
            Say "  [Environment]::SetEnvironmentVariable('Path', '$dir;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
        }
        return $dest
    } finally {
        if ($client) { $client.Dispose() }
        foreach ($path in @($owned)) { Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue }
        # Released only once this run's staging files are gone.
        if ($lock) { $lock.Dispose() }
        if ($tmpOwned) { Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue }
    }
}

# Only a script file may `exit`. As an in-memory script block, `exit` would end the user's session
# or calling script, and $PSCommandPath could be the caller's, so the mode is fixed here.
$LoadoutInstallAsFile = [bool]$MyInvocation.MyCommand.Path

function Complete([int]$Code) {
    if ($LoadoutInstallAsFile) { exit $Code }
    $global:LASTEXITCODE = $Code
}

try {
    $installed = Install-Loadout
} catch {
    Say "error: $($_.Exception.Message)"
    Complete 1
    return
}

if (-not $LoadoutUrl) {
    Complete 0
    return
}

# The init arguments aren't shown: they are the user's own, and could hold anything.
$more = if ($InitArgs.Count) { ' and the init arguments you gave' } else { '' }
Say "running $installed init $(Get-ShownUrl $LoadoutUrl)$more"
try {
    $initExit = Invoke-Native $installed (@('init', $LoadoutUrl) + $InitArgs)
} catch {
    Say "error: could not run $installed init: $($_.Exception.Message)"
    Complete 1
    return
}
Complete $initExit
