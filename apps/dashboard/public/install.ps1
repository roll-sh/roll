# Install the roll CLI (`roll.exe`) on Windows from the GitHub releases of roll-sh/roll:
#
#   irm https://roll.sh/install.ps1 | iex
#
# It picks the build for this CPU (x86_64 or ARM64), checks it against the release's SHA256SUMS,
# and puts `roll.exe` in %LOCALAPPDATA%\Programs\roll (on the user's PATH). For the machine (a
# server, or the local agent as a service: `roll secrets agent --install-service`), run it from an
# administrator's PowerShell with -System, or ROLL_INSTALL_SCOPE=machine:
#
#   & ([scriptblock]::Create((irm https://roll.sh/install.ps1))) -System
#
# That installs into %ProgramFiles%\roll and adds it to the machine's PATH. Settings (environment):
#   ROLL_VERSION        a release tag (`cli-v0.2.0`) or version (`0.2.0`); default: the newest cli-v* release
#   ROLL_INSTALL_DIR    where to put roll.exe (default: as above)
#   ROLL_INSTALL_SCOPE  `user` (default) or `machine` (as -System)
#   ROLL_RELEASES       where releases are downloaded from (default: https://github.com/roll-sh/roll/releases)
param([switch]$System)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Windows PowerShell 5.1 may still default to TLS 1.0.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = 'roll-sh/roll'
$Releases = if ($env:ROLL_RELEASES) { $env:ROLL_RELEASES.TrimEnd('/') } else { "https://github.com/$Repo/releases" }

function Say($msg) { [Console]::Error.WriteLine("roll: $msg") }
function Die($msg) { Say $msg; exit 1 }

$machine = $System -or ($env:ROLL_INSTALL_SCOPE -eq 'machine')
if ($machine) {
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $admin) { Die 'installing for the machine (-System) needs an administrator''s PowerShell' }
}
$Dir = if ($env:ROLL_INSTALL_DIR) { $env:ROLL_INSTALL_DIR } elseif ($machine) { Join-Path $env:ProgramFiles 'roll' } else { Join-Path $env:LOCALAPPDATA 'Programs\roll' }

# The OS's CPU, not this PowerShell's (an x64 PowerShell on ARM64 says AMD64).
$cpu = $null
try { $cpu = [string][System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture } catch { }
if (-not $cpu) { $cpu = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE } }
$arch = switch -regex ($cpu) {
    '^(X64|AMD64)$' { 'x86_64' }
    '^(Arm64|ARM64)$' { 'aarch64' }
    default { Die "no build for $cpu yet: build it from source (cargo install --git https://github.com/$Repo roll-cli)" }
}
$target = "$arch-pc-windows-msvc"

$tag = $env:ROLL_VERSION
if ($tag -and -not $tag.StartsWith('cli-v')) { $tag = if ($tag.StartsWith('v')) { "cli-$tag" } else { "cli-v$tag" } }
if (-not $tag) {
    try { $list = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases?per_page=50" } catch { Die "couldn't list the releases of ${Repo}: $_" }
    $tag = @($list | Where-Object { $_.tag_name -like 'cli-v*' } | Select-Object -First 1 -ExpandProperty tag_name)[0]
    if (-not $tag) { Die "no cli-v* release of $Repo yet" }
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("roll-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $asset = "roll-$target.zip"
    Say "installing $tag for $target into $Dir"
    try { Invoke-WebRequest -UseBasicParsing "$Releases/download/$tag/$asset" -OutFile (Join-Path $tmp $asset) } catch { Die "couldn't download $asset of ${tag}: $_" }
    try { Invoke-WebRequest -UseBasicParsing "$Releases/download/$tag/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS') } catch { Die "couldn't download the checksums of ${tag}: $_" }

    $want = $null
    foreach ($line in Get-Content (Join-Path $tmp 'SHA256SUMS')) {
        $f = $line -split '\s+', 2
        if ($f.Count -eq 2 -and ($f[1] -eq $asset -or $f[1] -eq "*$asset")) { $want = $f[0].ToLowerInvariant() }
    }
    if (-not $want) { Die "SHA256SUMS of $tag doesn't list $asset" }
    $have = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash.ToLowerInvariant()
    if ($have -ne $want) { Die "$asset doesn't match its checksum (got $have, want $want): not installing it" }

    Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath (Join-Path $tmp 'out')
    $new = Join-Path $tmp 'out\roll.exe'
    if (-not (Test-Path $new)) { Die "$asset has no roll.exe" }

    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $exe = Join-Path $Dir 'roll.exe'
    # A running roll.exe (the local agent's service) can't be overwritten, but it can be renamed:
    # it keeps running the old one until it restarts.
    if (Test-Path $exe) {
        $old = "$exe.old"
        Remove-Item -Force $old -ErrorAction SilentlyContinue
        Move-Item -Force $exe $old
    }
    Move-Item -Force $new $exe
    Remove-Item -Force "$exe.old" -ErrorAction SilentlyContinue
    Say "installed $(& $exe --version)"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$scope = if ($machine) { 'Machine' } else { 'User' }
$path = [string][Environment]::GetEnvironmentVariable('Path', $scope)
if (-not (($path -split ';') -contains $Dir)) {
    [Environment]::SetEnvironmentVariable('Path', (@($path.TrimEnd(';'), $Dir) -join ';').TrimStart(';'), $scope)
    $env:Path = "$env:Path;$Dir"
    Say "added $Dir to the $($scope.ToLower())'s PATH (new terminals have it)"
}
Say 'next: roll login https://your-roll'
