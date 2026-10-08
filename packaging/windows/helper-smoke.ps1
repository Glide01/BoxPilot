<#
Smoke-test BoxPilot's privileged helper on a real Windows machine (ADR 0006,
docs/helper-windows-checklist.md): install the MSI, check what it installed,
drive the BoxPilotHelper service with the smoke client
(crates/boxpilot-helper/examples/service_smoke.rs) as an administrator and as
a standard account, break the install on purpose, and uninstall.

  packaging/windows/helper-smoke.ps1 -Step <step> -Msi <BoxPilot.msi> `
      -Smoke <service_smoke.exe> [-Probe <token_probe.exe>] [-LogDir <dir>]

e.g. packaging/windows/helper-smoke.ps1 -Step all -Msi release/*.msi `
       -Smoke target/x86_64-pc-windows-msvc/release/examples/service_smoke.exe `
       -Probe target/x86_64-pc-windows-msvc/release/examples/token_probe.exe

Run it elevated, from the repository (it reads the helper's exit codes from
crates/boxpilot-protocol/src/endpoint.rs and the MSI's descriptors from
wix/main.wxs), on a machine you can throw away: it installs a service and the
wintun driver, creates and deletes a local account, brings TUN up for a few
seconds at a time (this machine's own traffic goes through it then), and
tampers with the installed files and ACLs (restoring each). CI runs it on
GitHub's windows-latest runner, a Windows Server whose runner is an elevated
administrator, one step per CI step:

  install          msiexec /i, silently
  inspect          the service's config and descriptor, the helper's files,
                   HelperState's descriptor
  token-probe      which token TUN needs, measured: the token probe
                   (crates/boxpilot-helper/examples/token_probe.rs) as SYSTEM
                   starts sing-box under one token after another, the first
                   adapter on this machine (wintun's driver install) with the
                   smallest, and the shipped tokenplan::SING_BOX_TOKEN on a
                   first install and in steady state; then the helper's own
                   token under what the SCM gives it (sc.exe privs), each
                   with a TUN run. Before `protocol`, whose first TUN start
                   would install the driver otherwise. It fails if the
                   shipped plans stop working or TUN needs a dangerous
                   privilege; narrower tokens failing are data.
  protocol         the smoke client as this administrator: hello, a refused
                   start, connection slots, the write deadline, three TUN runs
                   (one with probes and checks from outside while it runs,
                   sing-box's and the helper's tokens among them)
  idle-exit        the service stops by itself a minute after the last
                   connection, with exit code 0
  standard-user    the smoke client and sc.exe as a new standard
                   account: read-only, and kept out of everything else
  broken-install   a tampered sing-box.exe, a Users ACE on Helper, on
                   HelperState and on sing-box.exe, a squatted pipe name: each
                   stops the service with its exit code
  kill-helper      killing the helper during a TUN run kills sing-box
  uninstall        msiexec /x, silently; the service is gone, HelperState
                   stays
  logs             the helper's log, the MSI logs and the smoke client's
                   output (CI runs it always)
  all              every step above, in order, then logs

Each check is a function that throws with what it saw. -Msi may be a
wildcard naming exactly one file. -LogDir defaults to
$env:RUNNER_TEMP\boxpilot-helper-smoke (or $env:TEMP).

Not covered here (docs/helper-windows-checklist.md says what stays manual):
Windows 10 and 11 client SKUs, an unelevated administrator (a filtered token
can't be made non-interactively without a second logon), Network
Configuration Operators, the GUI, and what Process Explorer shows about
sing-box (its handles, environment and mitigations).
#>
[CmdletBinding()]
param(
    [ValidateSet('install', 'inspect', 'token-probe', 'protocol', 'idle-exit', 'standard-user',
        'broken-install', 'kill-helper', 'uninstall', 'logs', 'all')]
    [string] $Step = 'all',
    [string] $Msi = '',
    [string] $Smoke = '',
    [string] $Probe = '',
    [string] $LogDir = ''
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Native commands' exit codes are checked one by one: several checks expect
# one that isn't 0.
$PSNativeCommandUseErrorActionPreference = $false

# ---- Where things are ----

$RepoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
$EndpointSource = Join-Path $RepoRoot 'crates\boxpilot-protocol\src\endpoint.rs'
$WxsSource = Join-Path $RepoRoot 'wix\main.wxs'
$TokenPlanSource = Join-Path $RepoRoot 'crates\boxpilot-helper\src\tokenplan.rs'
$ProductDir = Join-Path $env:ProgramFiles 'BoxPilot'
$HelperDir = Join-Path $ProductDir 'Helper'
$StateDir = Join-Path $ProductDir 'HelperState'
$HelperLog = Join-Path $StateDir 'helper.log'
$ScExe = Join-Path $env:SystemRoot 'System32\sc.exe'
# Somewhere a standard account can run the smoke client from and write to.
$WorkDir = Join-Path $env:PUBLIC 'boxpilot-smoke'
$SmokeUser = 'bpsmoke'
# The token probe's work directory (no spaces: it is a scheduled task's
# argument), private to SYSTEM and Administrators as HelperState is, and its
# one-shot task.
$ProbeDir = Join-Path $env:SystemDrive 'boxpilot-token-probe'
$ProbeTask = 'BoxPilotTokenProbe'
# The probe's budget for its trials (--budget-secs), and how long this waits
# for its result: the budget, putting wintun's driver back after it, and the
# probe's own watchdog (the budget + 300 s).
$ProbeBudgetSec = 480
$ProbeWaitSec = 1080
if (-not $LogDir) {
    $base = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP }
    $LogDir = Join-Path $base 'boxpilot-helper-smoke'
}
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null

$SidSystem = 'S-1-5-18'
$SidAdministrators = 'S-1-5-32-544'
$SidUsers = 'S-1-5-32-545'
$SidTrustedInstaller = 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464'

# ---- What the repository says ----

# The service name and the helper's exit codes, as the GUI reads them:
# boxpilot_protocol::endpoint, never a guess.
function Read-Endpoint {
    $text = Get-Content -LiteralPath $EndpointSource -Raw
    $service = [regex]::Match($text, 'pub const SERVICE_NAME: &str = "([^"]+)";')
    if (-not $service.Success) {
        throw "no SERVICE_NAME in $EndpointSource"
    }
    $codes = @{}
    foreach ($match in [regex]::Matches($text, 'pub const ([A-Z_]+): i32 = (\d+);')) {
        $codes[$match.Groups[1].Value] = [int] $match.Groups[2].Value
    }
    foreach ($name in 'OK', 'HELPER_DIR_REFUSED', 'STATE_DIR_REFUSED', 'MANIFEST_REFUSED', 'PIPE_SQUATTED') {
        if (-not $codes.ContainsKey($name)) {
            throw "no exit code $name in $EndpointSource"
        }
    }
    [pscustomobject]@{ ServiceName = $service.Groups[1].Value; ExitCodes = $codes }
}

$Endpoint = Read-Endpoint
$ServiceName = $Endpoint.ServiceName
$ExitCodes = $Endpoint.ExitCodes

# The SDDL of a PermissionEx in wix/main.wxs.
function Get-WixSddl([string] $Id) {
    [xml] $wxs = Get-Content -LiteralPath $WxsSource -Raw
    $namespace = @{ wix = 'http://schemas.microsoft.com/wix/2006/wi' }
    $found = @(Select-Xml -Xml $wxs -XPath "//wix:PermissionEx[@Id='$Id']" -Namespace $namespace)
    if ($found.Count -ne 1) {
        throw "expected one PermissionEx $Id in $WxsSource, found $($found.Count)"
    }
    $found[0].Node.Sddl
}

function Get-MsiPath {
    if (-not $Msi) {
        throw 'this step needs -Msi'
    }
    $items = @(Get-ChildItem -Path $Msi -File)
    if ($items.Count -ne 1) {
        throw "-Msi $Msi names $($items.Count) files, not one"
    }
    $items[0].FullName
}

function Get-SmokeExe {
    if (-not $Smoke) {
        throw 'this step needs -Smoke'
    }
    (Resolve-Path -LiteralPath $Smoke).Path
}

function Get-ProbeExe {
    if (-not $Probe) {
        throw 'this step needs -Probe'
    }
    (Resolve-Path -LiteralPath $Probe).Path
}

# A privilege list as crates/boxpilot-helper/src/tokenplan.rs declares it
# (SING_BOX_PRIVILEGES, HELPER_PRIVILEGES), never a guess. Call it inside
# @(...).
function Read-PrivilegeList([string] $Name) {
    $text = Get-Content -LiteralPath $TokenPlanSource -Raw
    $match = [regex]::Match($text, 'pub const ' + $Name + ': &\[&str\] =\s*&\[(?<list>[^\]]*)\];')
    if (-not $match.Success) {
        throw "no $Name in $TokenPlanSource"
    }
    $names = @([regex]::Matches($match.Groups['list'].Value, '"(Se[A-Za-z]+Privilege)"') |
            ForEach-Object { $_.Groups[1].Value })
    if ($names.Count -eq 0) {
        throw "$Name in $TokenPlanSource names no privilege"
    }
    $names
}

# ---- Small things ----

function Write-Note([string] $Message) {
    if ($env:GITHUB_ACTIONS -eq 'true') {
        Write-Host "::warning title=helper smoke::$Message"
    } else {
        Write-Warning $Message
    }
}

# A native command's exit code and its whole output, stderr included, without
# a stderr line turning into an error (as it does in Windows PowerShell
# under 'Stop').
function Invoke-Native([string] $FilePath, [string[]] $Arguments) {
    $ErrorActionPreference = 'Continue'
    $output = & $FilePath @Arguments 2>&1 | ForEach-Object { "$_" } | Out-String
    [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = $output }
}

# sc.exe, by its full path (`sc` is Set-Content in Windows PowerShell), with
# its exit code (a Win32 error code) and output.
function Invoke-Sc {
    Invoke-Native $ScExe $args
}

# One `NAME : value` line of sc.exe's output.
function Get-ScField([string] $Text, [string] $Name) {
    $match = [regex]::Match($Text, '(?m)^\s*' + [regex]::Escape($Name) + '\s*:\s*(.*?)\s*$')
    if (-not $match.Success) {
        throw "sc.exe printed no $Name in:`n$Text"
    }
    $match.Groups[1].Value
}

# The ACEs of a descriptor's DACL (none for a NULL DACL).
function Get-DaclAces($Descriptor) {
    if ($null -eq $Descriptor.DiscretionaryAcl) {
        return
    }
    foreach ($ace in $Descriptor.DiscretionaryAcl) {
        $ace
    }
}

# The DACL of an SDDL string as sorted lines (type, flags, mask, SID), so two
# descriptors compare by meaning whatever order or spelling Windows chose.
# Like every function here that lists things, call it inside @(...): an
# empty or one-item list doesn't come out of a function as an array.
function Get-AceLines([string] $Sddl) {
    $descriptor = New-Object System.Security.AccessControl.RawSecurityDescriptor -ArgumentList $Sddl
    $lines = @(foreach ($ace in @(Get-DaclAces $descriptor)) {
            if ($ace -is [System.Security.AccessControl.KnownAce]) {
                '{0} {1} 0x{2:x8} {3}' -f $ace.AceType, $ace.AceFlags, $ace.AccessMask, $ace.SecurityIdentifier.Value
            } else {
                "unreadable ACE of type $($ace.AceType)"
            }
        })
    $lines | Sort-Object
}

# Owner, protection and DACL of a file or directory, in one comparable text.
function Get-SecuritySummary([string] $Path) {
    $descriptor = New-Object System.Security.AccessControl.RawSecurityDescriptor -ArgumentList (Get-Acl -LiteralPath $Path).Sddl
    $protected = ($descriptor.ControlFlags -band [System.Security.AccessControl.ControlFlags]::DiscretionaryAclProtected) -ne 0
    $owner = if ($descriptor.Owner) { $descriptor.Owner.Value } else { 'none' }
    "owner $owner; protected $protected; " + (@(Get-AceLines $descriptor.GetSddlForm('Access')) -join '; ')
}

# Whether an IP address lies in a prefix such as 172.18.0.1/30.
function Test-InPrefix([string] $Address, [string] $Prefix) {
    $network, $length = $Prefix.Split('/')
    $a = [System.Net.IPAddress]::Parse($Address).GetAddressBytes()
    $n = [System.Net.IPAddress]::Parse($network).GetAddressBytes()
    if ($a.Length -ne $n.Length) {
        return $false
    }
    $bits = [int] $length
    for ($i = 0; $i -lt $a.Length -and $bits -gt 8 * $i; $i++) {
        $take = [Math]::Min(8, $bits - 8 * $i)
        $mask = (0xFF -shl (8 - $take)) -band 0xFF
        if (($a[$i] -band $mask) -ne ($n[$i] -band $mask)) {
            return $false
        }
    }
    $true
}

function Assert-ProcessGone([int] $Id, [string] $What, [int] $TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while (Get-Process -Id $Id -ErrorAction SilentlyContinue) {
        if ((Get-Date) -gt $deadline) {
            throw "$What (pid $Id) still runs ${TimeoutSec}s later"
        }
        Start-Sleep -Milliseconds 200
    }
    Write-Host "ok: $What (pid $Id) is gone"
}

# ---- The service ----

function Get-HelperService {
    $query = Invoke-Sc query $ServiceName
    if ($query.ExitCode -ne 0) {
        throw "sc.exe query $ServiceName failed with $($query.ExitCode):`n$($query.Output)"
    }
    [pscustomobject]@{
        State           = (Get-ScField $query.Output 'STATE') -replace '^\d+\s+', ''
        Win32ExitCode   = [int] ((Get-ScField $query.Output 'WIN32_EXIT_CODE') -replace '\s.*$', '')
        ServiceExitCode = [int] ((Get-ScField $query.Output 'SERVICE_EXIT_CODE') -replace '\s.*$', '')
        Text            = $query.Output
    }
}

# The service's process; 0 when it isn't running.
function Get-HelperPid {
    $service = Get-CimInstance -ClassName Win32_Service -Filter "Name='$ServiceName'"
    if ($null -eq $service) {
        throw "there is no $ServiceName service"
    }
    [int] $service.ProcessId
}

function Wait-HelperStopped([int] $TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ($true) {
        $status = Get-HelperService
        if ($status.State -eq 'STOPPED') {
            return $status
        }
        if ((Get-Date) -gt $deadline) {
            throw "$ServiceName is still $($status.State) after ${TimeoutSec}s:`n$($status.Text)"
        }
        Start-Sleep -Milliseconds 500
    }
}

# Stop the service as this administrator, and wait until it has.
function Stop-HelperService {
    if ((Get-HelperService).State -ne 'STOPPED') {
        $stop = Invoke-Sc stop $ServiceName
        # 1061: it is already stopping; 1062: it already stopped.
        if (@(0, 1061, 1062) -notcontains $stop.ExitCode) {
            throw "sc.exe stop $ServiceName failed with $($stop.ExitCode):`n$($stop.Output)"
        }
    }
    Wait-HelperStopped 30 | Out-Null
}

# Start the stopped service, broken on purpose, and expect it to stop with
# one of the helper's exit codes (ERROR_SERVICE_SPECIFIC_ERROR, then the
# code), as the GUI reads it with QueryServiceStatus. $Documented is what
# endpoint.rs says this break gets; $Accepted may add others that still
# refuse to run, which are reported, not failed.
function Assert-StartRefused([string] $What, [string] $Documented, [string[]] $Accepted = @()) {
    $start = Invoke-Sc start $ServiceName
    Write-Host $start.Output
    # sc.exe start returns once the process runs, before the helper has
    # verified anything, so the stop that follows is the answer. Anything
    # but these means the start itself was refused.
    if (@(0, 1053, 1066) -notcontains $start.ExitCode) {
        throw "${What}: sc.exe start failed with $($start.ExitCode):`n$($start.Output)"
    }
    $status = Wait-HelperStopped 60
    $names = @($Documented) + $Accepted
    $got = $null
    foreach ($name in $names) {
        if ($status.Win32ExitCode -eq 1066 -and $status.ServiceExitCode -eq $ExitCodes[$name]) {
            $got = $name
            break
        }
    }
    if (-not $got) {
        $expected = ($names | ForEach-Object { "$_ ($($ExitCodes[$_]))" }) -join ' or '
        throw ("${What}: expected the service to stop with $expected, but it stopped with " +
            "Win32 exit code $($status.Win32ExitCode), service-specific $($status.ServiceExitCode):`n$($status.Text)")
    }
    if ($got -ne $Documented) {
        Write-Note ("${What}: the helper refused to run with $got ($($ExitCodes[$got])), while " +
            "boxpilot_protocol::endpoint::exit documents $Documented ($($ExitCodes[$Documented])) for it; " +
            'the GUI will name the wrong cause')
    }
    Write-Host "ok: ${What}: the service stopped with $got ($($ExitCodes[$got]))"
}

# ---- The smoke client ----

function Join-Arguments([string[]] $Arguments) {
    (@($Arguments | ForEach-Object {
            if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
        })) -join ' '
}

# Run the smoke client as this administrator; its output goes to the log.
# The arguments come as one array ('hello', '--expect', 'start'): bare
# `--expect` would be read as a parameter of this function.
function Invoke-Smoke([string[]] $Arguments) {
    $line = Join-Arguments $Arguments
    Write-Host "> service_smoke $line"
    & $script:SmokeExe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "service_smoke $line failed with exit code $LASTEXITCODE (its output is above)"
    }
}

# Start the smoke client in the background, its output in files: as this
# administrator, or with -Credential as that account.
function Start-SmokeBackground([string] $Name, [string[]] $Arguments, $Credential = $null) {
    $directory = if ($Credential) { $WorkDir } else { $LogDir }
    $exe = if ($Credential) { Join-Path $WorkDir 'service_smoke.exe' } else { $script:SmokeExe }
    $out = Join-Path $directory "$Name.out.txt"
    $err = Join-Path $directory "$Name.err.txt"
    $line = Join-Arguments $Arguments
    Write-Host "> service_smoke $line (in the background: $Name)"
    $start = @{
        FilePath               = $exe
        ArgumentList           = $line
        WorkingDirectory       = $directory
        RedirectStandardOutput = $out
        RedirectStandardError  = $err
        PassThru               = $true
    }
    if ($Credential) {
        $start.Credential = $Credential
    } else {
        $start.NoNewWindow = $true
    }
    $process = Start-Process @start
    # Holding the handle keeps the exit code readable once it has exited.
    $null = $process.Handle
    [pscustomobject]@{ Name = $Name; Process = $process; Out = $out; Err = $err }
}

function Show-SmokeOutput($Background) {
    foreach ($file in @($Background.Out, $Background.Err)) {
        if ((Test-Path -LiteralPath $file) -and (Get-Item -LiteralPath $file).Length -gt 0) {
            Write-Host "---- $($Background.Name): $(Split-Path -Leaf $file)"
            Get-Content -LiteralPath $file | ForEach-Object { Write-Host $_ }
        }
    }
}

# Wait for a background smoke client's ready file; its key=value lines.
function Wait-SmokeReady($Background, [string] $ReadyFile, [int] $TimeoutSec = 120) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while (-not (Test-Path -LiteralPath $ReadyFile)) {
        if ($Background.Process.HasExited) {
            Show-SmokeOutput $Background
            throw "service_smoke ($($Background.Name)) exited with $($Background.Process.ExitCode) before it was ready (its output is above)"
        }
        if ((Get-Date) -gt $deadline) {
            Show-SmokeOutput $Background
            throw "service_smoke ($($Background.Name)) wasn't ready within ${TimeoutSec}s (its output is above)"
        }
        Start-Sleep -Milliseconds 200
    }
    $values = Get-Content -LiteralPath $ReadyFile -Raw | ConvertFrom-StringData
    $shown = @($values.Keys | Sort-Object | ForEach-Object { "$_=$($values[$_])" }) -join ', '
    Write-Host "ready ($($Background.Name)): $shown"
    $values
}

# Let a background smoke client end (writing its release file first, if it
# has one), killing it if it won't, and show its output. Never throws, so it
# can run in `finally`; Assert-SmokeSucceeded then judges.
function Complete-SmokeBackground($Background, [string] $ReleaseFile, [int] $TimeoutSec = 60) {
    try {
        if ($ReleaseFile) {
            New-Item -ItemType File -Force -Path $ReleaseFile | Out-Null
        }
        if (-not $Background.Process.WaitForExit($TimeoutSec * 1000)) {
            Write-Warning "service_smoke ($($Background.Name)) still runs after ${TimeoutSec}s: killing it"
            $Background.Process.Kill()
            $Background.Process.WaitForExit()
        }
        Show-SmokeOutput $Background
    } catch {
        Write-Warning "ending service_smoke ($($Background.Name)): $($_.Exception.Message)"
    }
}

function Assert-SmokeSucceeded($Background) {
    $code = $Background.Process.ExitCode
    if ($code -ne 0) {
        throw "service_smoke ($($Background.Name)) failed with exit code $code (its output is above)"
    }
}

function Remove-Files([string[]] $Paths) {
    foreach ($path in $Paths) {
        Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    }
}

# ---- sing-box and TUN, from outside ----

function Get-HelperSingBoxProcesses {
    $path = Join-Path $HelperDir 'sing-box.exe'
    @(Get-CimInstance -ClassName Win32_Process -Filter "Name='sing-box.exe'" |
            Where-Object { $_.ExecutablePath -and $_.ExecutablePath -ieq $path })
}

function Get-RunDirs {
    @(Get-ChildItem -LiteralPath (Join-Path $StateDir 'runs') -Force -ErrorAction SilentlyContinue)
}

# The helper's sing-box: one process, the helper's child, the installed
# binary, running in a run directory under HelperState.
function Assert-SingBoxUnderHelper([int] $HelperPid) {
    $path = Join-Path $HelperDir 'sing-box.exe'
    $runs = Join-Path $StateDir 'runs'
    $all = @(Get-CimInstance -ClassName Win32_Process -Filter "Name='sing-box.exe'")
    $all | Format-Table ProcessId, ParentProcessId, ExecutablePath -AutoSize | Out-String | Write-Host
    $ours = @($all | Where-Object { $_.ParentProcessId -eq $HelperPid })
    if ($ours.Count -ne 1) {
        throw "expected one sing-box.exe whose parent is the helper (pid $HelperPid), found $($ours.Count) (all sing-box.exe processes are above)"
    }
    $singBox = $ours[0]
    if ($singBox.ExecutablePath -ine $path) {
        throw "the helper's sing-box runs $($singBox.ExecutablePath), not $path"
    }
    if (-not $singBox.CommandLine -or -not $singBox.CommandLine.Contains("$runs\")) {
        throw "sing-box's command line names no run directory under ${runs}: $($singBox.CommandLine)"
    }
    Write-Host "ok: sing-box (pid $($singBox.ProcessId)) is the helper's child, runs $path in a run directory: $($singBox.CommandLine)"
    $singBox
}

# The TUN adapter while TUN is up: the interface holding the TUN address is
# up, and its PnP friendly name is the one the helper's crash cleanup
# matches (`tun::is_sing_tun_friendly_name`).
function Assert-TunAdapterUp([string] $TunAddress) {
    Get-NetAdapter -IncludeHidden | Format-Table Name, InterfaceDescription, Status, ifIndex -AutoSize | Out-String | Write-Host
    $ip = @(Get-NetIPAddress -IPAddress $TunAddress -ErrorAction SilentlyContinue)
    if ($ip.Count -eq 0) {
        throw "no interface holds the TUN address $TunAddress while TUN is up"
    }
    $adapter = Get-NetAdapter -InterfaceIndex $ip[0].InterfaceIndex -IncludeHidden
    $device = Get-PnpDevice -InstanceId $adapter.PnPDeviceID
    if ($adapter.Status -ne 'Up') {
        throw "the TUN adapter $($adapter.Name) is $($adapter.Status), not Up"
    }
    if ($device.FriendlyName -notlike 'sing-tun*') {
        throw ("the TUN adapter's friendly name is '$($device.FriendlyName)': the helper's cleanup " +
            "(tun::is_sing_tun_friendly_name) would never remove it after a crash")
    }
    Write-Host "ok: TUN adapter $($adapter.Name) ($($device.FriendlyName)) is up with $TunAddress"
}

# Nothing listens on TCP but sing-box, and sing-box only on loopback and its
# TUN address: no LAN host reaches anything (Allow LAN is off in the smoke
# profile).
function Assert-Listeners([int] $HelperPid, [int] $SingBoxPid, [string] $TunPrefix) {
    $helperTcp = @(Get-NetTCPConnection -State Listen -OwningProcess $HelperPid -ErrorAction SilentlyContinue)
    if ($helperTcp.Count -gt 0) {
        $list = ($helperTcp | ForEach-Object { "$($_.LocalAddress):$($_.LocalPort)" }) -join ', '
        throw "the helper itself listens on TCP: $list"
    }
    $singBoxTcp = @(Get-NetTCPConnection -State Listen -OwningProcess $SingBoxPid -ErrorAction SilentlyContinue)
    $singBoxTcp | Format-Table LocalAddress, LocalPort -AutoSize | Out-String | Write-Host
    $wide = @($singBoxTcp | Where-Object {
            -not (Test-InPrefix $_.LocalAddress '127.0.0.0/8') -and $_.LocalAddress -ne '::1' -and
            -not (Test-InPrefix $_.LocalAddress $TunPrefix)
        })
    if ($wide.Count -gt 0) {
        $list = ($wide | ForEach-Object { "$($_.LocalAddress):$($_.LocalPort)" }) -join ', '
        throw "sing-box listens beyond loopback and its TUN address: $list"
    }
    Write-Host "ok: the helper listens on no TCP port; sing-box on loopback and $TunPrefix only"
}

# A token read from outside by the smoke client as this administrator
# (`service_smoke token --expect <Expect>`). If an administrator can't read
# it at all (exit code 3), that is said, not failed.
function Assert-Token([string] $What, [int] $ProcessId, [string] $Expect, [string] $Plan) {
    $check = Invoke-Native $script:SmokeExe @('token', '--pid', "$ProcessId", '--expect', $Expect)
    Write-Host $check.Output
    switch ($check.ExitCode) {
        0 {
            Write-Host "ok: $What's token (pid $ProcessId) is what $Plan plans"
        }
        3 {
            Write-Note ("this administrator could not read $What's token (pid $ProcessId) from " +
                "outside, so it is not checked; the smoke client's output says why")
        }
        default {
            throw "$What's token (pid $ProcessId) is not what $Plan plans (the smoke client's output is above)"
        }
    }
}

# While TUN runs: sing-box's token is exactly what tokenplan::SING_BOX_TOKEN
# plans (privileges, integrity level, Administrators), and the helper's own
# holds exactly tokenplan::HELPER_TOKEN's privileges, which it dropped to
# when it started.
function Assert-SingBoxToken([int] $SingBoxPid, [int] $HelperPid) {
    Assert-Token 'sing-box' $SingBoxPid 'sing-box' 'tokenplan::SING_BOX_TOKEN'
    Assert-Token 'the helper' $HelperPid 'helper' 'tokenplan::HELPER_TOKEN'
}

function Get-SingTunDevices([switch] $PresentOnly) {
    @(Get-PnpDevice -Class Net -PresentOnly:$PresentOnly -ErrorAction SilentlyContinue |
            Where-Object { $_.FriendlyName -like 'sing-tun*' })
}

# After a run: no sing-box from the helper's directory, no sing-tun adapter
# present (the machine's routes are its own again), and (unless the helper
# died with it) no run directory.
function Assert-TunDown([switch] $AllowRunDirs) {
    $deadline = (Get-Date).AddSeconds(20)
    while ($true) {
        $singBox = @(Get-HelperSingBoxProcesses)
        $adapters = @(Get-SingTunDevices -PresentOnly)
        $runs = @(if (-not $AllowRunDirs) { Get-RunDirs })
        if ($singBox.Count -eq 0 -and $adapters.Count -eq 0 -and $runs.Count -eq 0) {
            break
        }
        if ((Get-Date) -gt $deadline) {
            throw ("20s after the run: $($singBox.Count) sing-box.exe from $HelperDir still run, " +
                "$($adapters.Count) sing-tun adapters are present, $($runs.Count) run directories remain")
        }
        Start-Sleep -Milliseconds 500
    }
    Write-Host 'ok: sing-box is gone, no sing-tun adapter is present, no run directory is left'
}

# sing-tun adapters installed but no longer present: the helper removes them
# after each run and when it starts.
function Test-StaleAdapters {
    $stale = @(Get-SingTunDevices | Where-Object { -not $_.Present })
    if ($stale.Count -gt 0) {
        $names = ($stale | ForEach-Object { "$($_.FriendlyName) [$($_.InstanceId)]" }) -join ', '
        Write-Note "sing-tun adapters that are no longer present remain installed: $names"
    } else {
        Write-Host 'ok: no stale sing-tun adapter is installed'
    }
}

# ---- Descriptors ----

function Assert-ServiceConfig {
    $config = Invoke-Sc qc $ServiceName
    if ($config.ExitCode -ne 0) {
        throw "sc.exe qc $ServiceName failed with $($config.ExitCode):`n$($config.Output)"
    }
    Write-Host $config.Output
    $path = Get-ScField $config.Output 'BINARY_PATH_NAME'
    $expected = '"' + (Join-Path $HelperDir 'boxpilot-helper.exe') + '"'
    if ($path -ne $expected) {
        throw "the service runs $path; expected exactly $expected (quoted, in the fixed helper directory, no arguments)"
    }
    $type = Get-ScField $config.Output 'TYPE'
    if ($type -notmatch 'WIN32_OWN_PROCESS') {
        throw "the service's type is $type, not WIN32_OWN_PROCESS"
    }
    $start = Get-ScField $config.Output 'START_TYPE'
    if ($start -notmatch 'DEMAND_START') {
        throw "the service starts $start, not DEMAND_START"
    }
    $account = Get-ScField $config.Output 'SERVICE_START_NAME'
    if ($account -ne 'LocalSystem') {
        throw "the service runs as $account, not LocalSystem"
    }
    Write-Host 'ok: quoted path in the helper directory, own process, demand start, LocalSystem'
}

# The service's DACL is the MSI's (wix/main.wxs, HelperServiceSecurity),
# compared ACE by ACE: Windows may reorder or respell it. sc.exe sdshow shows
# no owner, and its SACL is Windows' own, so only the DACL is compared.
function Assert-ServiceSddl {
    $expected = Get-WixSddl 'HelperServiceSecurity'
    $shown = Invoke-Sc sdshow $ServiceName
    if ($shown.ExitCode -ne 0) {
        throw "sc.exe sdshow $ServiceName failed with $($shown.ExitCode):`n$($shown.Output)"
    }
    $actual = $shown.Output.Trim()
    $want = @(Get-AceLines $expected)
    $have = @(Get-AceLines $actual)
    $difference = @(Compare-Object -ReferenceObject $want -DifferenceObject $have)
    if ($difference.Count -gt 0) {
        $diff = ($difference | ForEach-Object { "$($_.SideIndicator) $($_.InputObject)" }) -join "`n"
        throw ("the service's DACL differs from the MSI's.`n wix:    $expected`n sdshow: $actual`n" +
            "(<= only in wix, => only on the service)`n$diff")
    }
    Write-Host "ok: the service's DACL is the MSI's: $actual"
}

# The helper directory holds exactly what the manifest names, besides the
# helper and the manifest, each with the manifest's hash.
function Assert-HelperFiles {
    $manifest = Get-Content -LiteralPath (Join-Path $HelperDir 'manifest.json') -Raw | ConvertFrom-Json
    if ($manifest.sing_box.file -ne 'sing-box.exe') {
        throw "the manifest names sing-box $($manifest.sing_box.file)"
    }
    $listed = @($manifest.sing_box) + @($manifest.extra_files)
    $expected = @('boxpilot-helper.exe', 'manifest.json') + @($listed | ForEach-Object { $_.file }) | Sort-Object
    $actual = @(Get-ChildItem -LiteralPath $HelperDir -Force | ForEach-Object { $_.Name }) | Sort-Object
    if (($expected -join '|') -cne ($actual -join '|')) {
        throw "$HelperDir holds [$($actual -join ', ')], expected exactly [$($expected -join ', ')]"
    }
    foreach ($entry in $listed) {
        $hash = (Get-FileHash -LiteralPath (Join-Path $HelperDir $entry.file) -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($hash -cne $entry.sha256) {
            throw "$($entry.file) hashes to $hash, the manifest says $($entry.sha256)"
        }
    }
    Write-Host "ok: $HelperDir holds exactly $($actual -join ', '), as the manifest hashes them"
    foreach ($path in @(($env:SystemDrive + '\'), $env:ProgramFiles, $ProductDir, $HelperDir)) {
        Write-Host "---- icacls $path"
        & icacls.exe $path | ForEach-Object { Write-Host $_ }
    }
}

# HelperState as the MSI made it: owned by SYSTEM, protected, and exactly
# the MSI's DACL (wix/main.wxs, HelperStateSecurity).
function Assert-StateDirDescriptor {
    $expected = Get-WixSddl 'HelperStateSecurity'
    $actual = (Get-Acl -LiteralPath $StateDir).Sddl
    $descriptor = New-Object System.Security.AccessControl.RawSecurityDescriptor -ArgumentList $actual
    if ($descriptor.Owner.Value -ne $SidSystem) {
        throw "HelperState is owned by $($descriptor.Owner.Value), not SYSTEM: $actual"
    }
    if (($descriptor.ControlFlags -band [System.Security.AccessControl.ControlFlags]::DiscretionaryAclProtected) -eq 0) {
        throw "HelperState's DACL is not protected (it inherits from Program Files): $actual"
    }
    $difference = @(Compare-Object -ReferenceObject @(Get-AceLines $expected) -DifferenceObject @(Get-AceLines $actual))
    if ($difference.Count -gt 0) {
        throw "HelperState's DACL differs from the MSI's.`n wix:     $expected`n Get-Acl: $actual"
    }
    Write-Host "ok: HelperState is SYSTEM's, protected, SYSTEM and Administrators only: $actual"
}

# Everything in HelperState once the helper has used it (its log, runs,
# users\<SID> and the cache in it): owned by an administrator, and only
# SYSTEM and Administrators granted anything.
function Assert-StateTreePrivate {
    $items = @(Get-Item -LiteralPath $StateDir) + @(Get-ChildItem -LiteralPath $StateDir -Recurse -Force)
    foreach ($item in $items) {
        $descriptor = New-Object System.Security.AccessControl.RawSecurityDescriptor -ArgumentList (Get-Acl -LiteralPath $item.FullName).Sddl
        $owner = $descriptor.Owner.Value
        if (@($SidSystem, $SidAdministrators, $SidTrustedInstaller) -notcontains $owner) {
            throw "$($item.FullName) is owned by $owner"
        }
        foreach ($ace in @(Get-DaclAces $descriptor)) {
            if ($ace -isnot [System.Security.AccessControl.QualifiedAce]) {
                throw "$($item.FullName) has an ACE of type $($ace.AceType) this check can't judge"
            }
            $allows = $ace.AceQualifier -eq [System.Security.AccessControl.AceQualifier]::AccessAllowed
            if ($allows -and @($SidSystem, $SidAdministrators) -notcontains $ace.SecurityIdentifier.Value) {
                throw "$($item.FullName) grants $($ace.SecurityIdentifier.Value) access 0x$('{0:x8}' -f $ace.AccessMask)"
            }
        }
    }
    Write-Host "---- icacls $StateDir /T"
    & icacls.exe $StateDir /T /C | ForEach-Object { Write-Host $_ }
    Write-Host "ok: everything in HelperState ($($items.Count) entries) is SYSTEM's and Administrators' only"
}

# Add an ACE for Users to $Path, run $Body, then take the ACE away and check
# the descriptor is as it was. The new descriptor is written through .NET,
# which writes only the DACL it changed (Set-Acl may try the owner too).
function Invoke-WithUsersAce([string] $Path, [string] $Rights, [scriptblock] $Body) {
    $before = Get-SecuritySummary $Path
    $users = New-Object System.Security.Principal.SecurityIdentifier -ArgumentList $SidUsers
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule -ArgumentList $users, $Rights, 'Allow'
    $acl = Get-Acl -LiteralPath $Path
    $acl.AddAccessRule($rule)
    Set-PathAcl $Path $acl
    Write-Host "added Users ($Rights) to $Path"
    try {
        & $Body
    } finally {
        $acl = Get-Acl -LiteralPath $Path
        [void] $acl.RemoveAccessRule($rule)
        Set-PathAcl $Path $acl
        $after = Get-SecuritySummary $Path
        if ($after -ne $before) {
            throw "restoring the descriptor of $Path failed:`n before: $before`n after:  $after"
        }
        Write-Host "restored the descriptor of $Path"
    }
}

function Set-PathAcl([string] $Path, $Acl) {
    $item = Get-Item -LiteralPath $Path -Force
    $info = if ($item.PSIsContainer) {
        New-Object System.IO.DirectoryInfo -ArgumentList $item.FullName
    } else {
        New-Object System.IO.FileInfo -ArgumentList $item.FullName
    }
    if ('System.IO.FileSystemAclExtensions' -as [type]) {
        [System.IO.FileSystemAclExtensions]::SetAccessControl($info, $Acl)
    } else {
        $info.SetAccessControl($Acl)
    }
}

# ---- The standard account ----

# A password nobody sees: CSPRNG, one of each class first so any complexity
# policy is met. Never printed, never on a command line.
function New-Password {
    $alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789'
    $bytes = New-Object byte[] 24
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($bytes)
    } finally {
        $rng.Dispose()
    }
    'Aa9-' + (-join ($bytes | ForEach-Object { $alphabet[$_ % $alphabet.Length] }))
}

# Create the standard account through ADSI, so its password is never on a
# command line, and put it in Users (which may log on locally).
function New-SmokeUser([string] $Password) {
    $computer = [ADSI] "WinNT://$env:COMPUTERNAME,computer"
    $user = $computer.psbase.Children.Add($SmokeUser, 'User')
    [void] $user.psbase.Invoke('SetPassword', $Password)
    $user.psbase.CommitChanges()
    # A logon that must change its password first would fail below.
    try {
        $user.psbase.InvokeSet('PasswordExpired', 0)
        $user.psbase.CommitChanges()
    } catch {
        Write-Warning "clearing PasswordExpired on ${SmokeUser}: $($_.Exception.Message)"
    }
    $users = (New-Object System.Security.Principal.SecurityIdentifier -ArgumentList $SidUsers).Translate(
        [System.Security.Principal.NTAccount]).Value.Split('\')[-1]
    $add = Invoke-Native 'net.exe' @('localgroup', $users, $SmokeUser, '/add')
    # 1378: already a member.
    if ($add.ExitCode -ne 0 -and $add.Output -notmatch '1378') {
        throw "net localgroup $users $SmokeUser /add failed with $($add.ExitCode): $($add.Output)"
    }
    Write-Host "created the standard account $SmokeUser (in $users only)"
}

function Remove-SmokeUser {
    $delete = Invoke-Native 'net.exe' @('user', $SmokeUser, '/delete')
    if ($delete.ExitCode -ne 0) {
        Write-Warning "net user $SmokeUser /delete failed with $($delete.ExitCode): $($delete.Output)"
    } else {
        Write-Host "deleted the standard account $SmokeUser"
    }
}

# Run a program as the standard account (an interactive logon, as
# CreateProcessWithLogonW makes, so its token holds INTERACTIVE, which the
# pipe's and the service's DACLs name), and return its exit code and output.
function Invoke-AsUser([string] $Name, [string] $FilePath, [string] $ArgumentString, $Credential, [int] $TimeoutSec = 120) {
    $out = Join-Path $WorkDir "$Name.out.txt"
    $err = Join-Path $WorkDir "$Name.err.txt"
    Write-Host "> as ${SmokeUser}: $(Split-Path -Leaf $FilePath) $ArgumentString"
    $process = Start-Process -FilePath $FilePath -ArgumentList $ArgumentString -Credential $Credential `
        -WorkingDirectory $WorkDir -RedirectStandardOutput $out -RedirectStandardError $err -PassThru
    $null = $process.Handle
    if (-not $process.WaitForExit($TimeoutSec * 1000)) {
        $process.Kill()
        throw "as ${SmokeUser}, $FilePath $ArgumentString didn't finish within ${TimeoutSec}s"
    }
    $output = ((Get-Content -LiteralPath $out -Raw -ErrorAction SilentlyContinue), (Get-Content -LiteralPath $err -Raw -ErrorAction SilentlyContinue)) -join ''
    if ($output) {
        Write-Host $output.TrimEnd()
    }
    [pscustomobject]@{ ExitCode = $process.ExitCode; Output = $output }
}

function Invoke-SmokeAsUser([string] $Name, [string] $ArgumentString, $Credential) {
    $result = Invoke-AsUser $Name (Join-Path $WorkDir 'service_smoke.exe') $ArgumentString $Credential
    if ($result.ExitCode -ne 0) {
        throw "as ${SmokeUser}, service_smoke $ArgumentString failed with exit code $($result.ExitCode) (its output is above)"
    }
}

# sc.exe as the standard account: it may query and start the service (the
# DACL's RP for interactive users), never stop it, change its config or
# rewrite its descriptor. sdset is tried with the service's own DACL, so even
# a wrongly granted write would change nothing.
function Assert-UserServiceRights($Credential) {
    $dacl = ((Invoke-Sc sdshow $ServiceName).Output.Trim() -csplit 'S:')[0]
    $checks = @(
        @{ Name = 'sc-query'; Arguments = "query $ServiceName"; Expect = @(0); Why = 'may query it' },
        @{ Name = 'sc-start'; Arguments = "start $ServiceName"; Expect = @(0, 1056); Why = 'may start it' },
        @{ Name = 'sc-stop'; Arguments = "stop $ServiceName"; Expect = @(5); Why = 'may not stop it' },
        @{ Name = 'sc-config'; Arguments = "config $ServiceName start= demand"; Expect = @(5); Why = 'may not change its config' },
        @{ Name = 'sc-sdset'; Arguments = "sdset $ServiceName $dacl"; Expect = @(5); Why = 'may not rewrite its descriptor' }
    )
    foreach ($check in $checks) {
        $result = Invoke-AsUser $check.Name $ScExe $check.Arguments $Credential
        if ($check.Expect -notcontains $result.ExitCode) {
            throw "as ${SmokeUser}, sc.exe $($check.Arguments) exited with $($result.ExitCode), expected $($check.Expect -join ' or ') ($($check.Why))"
        }
        Write-Host "ok: a standard account $($check.Why) (sc.exe exit code $($result.ExitCode))"
    }
}

# HelperState and the helper's log can't even be listed or read.
function Assert-UserStateAccess($Credential) {
    Invoke-SmokeAsUser 'denied' "denied --dir `"$StateDir`" --file `"$HelperLog`"" $Credential
}

# Four read-only connections held by the standard account, a fifth closed,
# and meanwhile an administrator still gets in.
function Assert-ReadOnlySlots($Credential) {
    $ready = Join-Path $WorkDir 'readonly.ready'
    $release = Join-Path $WorkDir 'readonly.release'
    Remove-Files @($ready, $release)
    $background = Start-SmokeBackground 'readonly-slots' @('readonly-slots', '--ready-file', $ready, '--release-file', $release) $Credential
    try {
        Wait-SmokeReady $background $ready | Out-Null
        Invoke-Smoke 'hello', '--expect', 'start'
        Write-Host 'ok: an administrator is served while a standard account holds every read-only slot'
    } finally {
        Complete-SmokeBackground $background $release
    }
    Assert-SmokeSucceeded $background
}

# ---- The steps ----

function Invoke-InstallStep {
    $msiPath = Get-MsiPath
    $log = Join-Path $LogDir 'msi-install.log'
    Write-Host "installing $msiPath (log: $log)"
    $process = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\msiexec.exe') `
        -ArgumentList "/i `"$msiPath`" /qn /norestart /l*v `"$log`"" -Wait -PassThru
    if (@(0, 3010) -notcontains $process.ExitCode) {
        Show-MsiLog $log
        throw "msiexec /i exited with $($process.ExitCode) (the log's errors are above)"
    }
    if ($process.ExitCode -eq 3010) {
        Write-Note 'the MSI asked for a restart'
    }
    $status = Get-HelperService
    if ($status.State -ne 'STOPPED') {
        throw "right after the install $ServiceName is $($status.State); the MSI must not start it"
    }
    Write-Host "ok: installed; $ServiceName exists and is stopped"
}

function Invoke-InspectStep {
    Assert-ServiceConfig
    Assert-ServiceSddl
    Assert-HelperFiles
    Assert-StateDirDescriptor
}

# One TUN run with the probes, checked from outside while it is up: the
# adapter, sing-box's parent, binary and run directory, who listens where.
function Invoke-ProbedTunRun {
    $ready = Join-Path $LogDir 'tun.ready'
    $release = Join-Path $LogDir 'tun.release'
    Remove-Files @($ready, $release)
    # The probes resolve a name through TUN: not from the cache.
    Clear-DnsClientCache
    $background = Start-SmokeBackground 'tun-probes' @('tun', '--probes', '--ready-file', $ready, '--release-file', $release, '--end', 'stop')
    try {
        $values = Wait-SmokeReady $background $ready
        $helperPid = Get-HelperPid
        if ([int] $values['helper_pid'] -ne $helperPid) {
            throw "the pipe's server is pid $($values['helper_pid']), the service's process is $helperPid"
        }
        Assert-TunAdapterUp $values['tun_address']
        $singBox = Assert-SingBoxUnderHelper $helperPid
        Assert-SingBoxToken $singBox.ProcessId $helperPid
        Assert-Listeners $helperPid $singBox.ProcessId $values['tun_prefix']
        $runs = @(Get-RunDirs)
        if ($runs.Count -ne 1) {
            throw "while one sing-box runs, HelperState\runs holds $($runs.Count) entries"
        }
        Write-Host "ok: one run directory: $($runs[0].Name)"
    } finally {
        Complete-SmokeBackground $background $release
    }
    Assert-SmokeSucceeded $background
    Assert-TunDown
}

# ---- The token probe ----

# Print the lines of $Path after the first $Printed; how many there are now.
# While the probe still writes, its last line may be half written, so it
# waits for the next look unless -Complete.
function Show-NewLines([string] $Path, [int] $Printed, [switch] $Complete) {
    if (-not (Test-Path -LiteralPath $Path)) {
        return $Printed
    }
    $lines = @(Get-Content -LiteralPath $Path -ErrorAction SilentlyContinue)
    $upTo = if ($Complete) { $lines.Count } else { $lines.Count - 1 }
    for ($i = $Printed; $i -lt $upTo; $i++) {
        Write-Host $lines[$i]
    }
    [Math]::Max($Printed, $upTo)
}

# Run the token probe as SYSTEM, as the helper runs: a one-shot scheduled
# task (the ScheduledTasks module: no command line to quote, nothing left
# registered afterwards), its log shown as it goes, until it writes `done`.
# Returns what `done` says: `ok`, or `malfunction: ...`.
function Invoke-ProbeAsSystem {
    $exe = Join-Path $ProbeDir 'token_probe.exe'
    $log = Join-Path $ProbeDir 'probe.log'
    $done = Join-Path $ProbeDir 'done'
    $argument = "--work `"$ProbeDir`" --budget-secs $ProbeBudgetSec"
    $action = New-ScheduledTaskAction -Execute $exe -Argument $argument -WorkingDirectory $ProbeDir
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds ($ProbeWaitSec + 120)) `
        -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew
    Unregister-ScheduledTask -TaskName $ProbeTask -Confirm:$false -ErrorAction SilentlyContinue
    Register-ScheduledTask -TaskName $ProbeTask -Action $action -Principal $principal -Settings $settings | Out-Null
    $grouped = $env:GITHUB_ACTIONS -eq 'true'
    try {
        Write-Host "running $exe $argument as SYSTEM (scheduled task $ProbeTask)"
        $began = Get-Date
        Start-ScheduledTask -TaskName $ProbeTask
        if ($grouped) {
            Write-Host '::group::token probe: its log, as it runs'
        }
        $printed = 0
        $notRunningSince = $null
        while (-not (Test-Path -LiteralPath $done)) {
            $printed = Show-NewLines $log $printed
            $elapsed = ((Get-Date) - $began).TotalSeconds
            if ($elapsed -gt $ProbeWaitSec) {
                throw "the token probe wrote no result within ${ProbeWaitSec}s"
            }
            # A probe that died without a word (a panic, a kill) leaves its
            # task not running and no `done`.
            $state = "$((Get-ScheduledTask -TaskName $ProbeTask).State)"
            if ($state -eq 'Running' -or $elapsed -lt 15) {
                $notRunningSince = $null
            } elseif (-not $notRunningSince) {
                $notRunningSince = Get-Date
            } elseif (((Get-Date) - $notRunningSince).TotalSeconds -gt 10) {
                $info = Get-ScheduledTaskInfo -TaskName $ProbeTask
                throw "the token probe's task is $state (last result $($info.LastTaskResult)), and the probe wrote no result"
            }
            Start-Sleep -Seconds 2
        }
        # `done` comes last: whatever it logged is in.
        Start-Sleep -Seconds 1
        $null = Show-NewLines $log $printed -Complete
    } finally {
        if ($grouped) {
            Write-Host '::endgroup::'
        }
        $info = Get-ScheduledTaskInfo -TaskName $ProbeTask -ErrorAction SilentlyContinue
        if ($info) {
            Write-Host "token probe task: last result $($info.LastTaskResult)"
        }
        Stop-ScheduledTask -TaskName $ProbeTask -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $ProbeTask -Confirm:$false -ErrorAction SilentlyContinue
    }
    (Get-Content -LiteralPath $done -Raw).Trim()
}

# `sc.exe privs`: the service's required privileges
# (SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO). The SCM gives the helper's
# process only these when it next starts; the helper then drops what of
# them tokenplan::HELPER_TOKEN doesn't keep.
function Set-HelperPrivileges([string[]] $Privileges) {
    Stop-HelperService
    $set = Invoke-Sc privs $ServiceName (@($Privileges) -join '/')
    if ($set.ExitCode -ne 0) {
        throw "sc.exe privs $ServiceName failed with $($set.ExitCode):`n$($set.Output)"
    }
}

function Get-TokenVerdict([int] $ExitCode, [string] $Held) {
    switch ($ExitCode) {
        0 { $Held }
        3 { 'unreadable' }
        default { "differs ($ExitCode)" }
    }
}

# One helper token trial: the SCM's list set (or, with -Scm '', left as it
# is), then a whole TUN run through the real service (the pipe and the
# caller's token, the job, CreateProcessAsUserW with sing-box's restricted
# token, the adapter cleanup after it), with both tokens read from outside
# while it runs. The helper's must be exactly -HelperHolds (comma-separated)
# or, with -HelperHolds '', exactly tokenplan::HELPER_TOKEN's privileges.
# Never throws: what happened is the row.
function Invoke-HelperTokenTrial([int] $Index, [string] $Name, [string] $Scm, [string] $HelperHolds) {
    Write-Host "---- helper token trial ${Index}: $Name"
    $row = [ordered]@{
        '#'          = $Index
        'SCM gives'  = $Name
        'starts'     = 'no'
        'TUN run'    = '-'
        'its token'  = '-'
        'sing-box'   = '-'
        'cleanup'    = '-'
    }
    if ($Scm) {
        try {
            Set-HelperPrivileges @($Scm -split '/')
        } catch {
            $row['starts'] = 'sc privs failed'
            Write-Host $_.Exception.Message
            return [pscustomobject] $row
        }
    } else {
        Stop-HelperService
    }
    Write-Host "---- sc.exe qprivs $ServiceName"
    Write-Host (Invoke-Sc qprivs $ServiceName).Output
    $ready = Join-Path $LogDir "helper-token-$Index.ready"
    $release = Join-Path $LogDir "helper-token-$Index.release"
    Remove-Files @($ready, $release)
    $background = Start-SmokeBackground "helper-token-$Index" @('tun', '--ready-file', $ready, '--release-file', $release, '--end', 'stop')
    try {
        $null = Wait-SmokeReady $background $ready 90
        $row['starts'] = 'yes'
        $helperPid = Get-HelperPid
        $expect = if ($HelperHolds) {
            @('--expect', 'privileges', '--privileges', $HelperHolds)
        } else {
            @('--expect', 'helper')
        }
        $check = Invoke-Native $script:SmokeExe (@('token', '--pid', "$helperPid") + $expect)
        Write-Host $check.Output
        $row['its token'] = Get-TokenVerdict $check.ExitCode 'as expected'
        $singBox = @(Get-CimInstance -ClassName Win32_Process -Filter "Name='sing-box.exe'" |
                Where-Object { $_.ParentProcessId -eq $helperPid })
        if ($singBox.Count -eq 1) {
            $check = Invoke-Native $script:SmokeExe @('token', '--pid', "$($singBox[0].ProcessId)", '--expect', 'sing-box')
            Write-Host $check.Output
            $row['sing-box'] = Get-TokenVerdict $check.ExitCode 'as planned'
        }
    } catch {
        Write-Host "helper token trial ${Index}: $($_.Exception.Message)"
        $status = Get-HelperService
        if ($status.State -eq 'STOPPED' -and $status.Win32ExitCode -ne 0) {
            $row['starts'] = "no: exit $($status.Win32ExitCode)/$($status.ServiceExitCode)"
        }
    } finally {
        Complete-SmokeBackground $background $release
    }
    $code = $background.Process.ExitCode
    $row['TUN run'] = if ($code -eq 0) { 'works' } else { "fails ($code)" }
    # The helper removes sing-box's adapter after the run (DiUninstallDevice
    # as its own, dropped token): none may be left.
    $stale = @(Get-SingTunDevices | Where-Object { -not $_.Present })
    $row['cleanup'] = if ($stale.Count -eq 0) { 'none stale' } else { "$($stale.Count) stale left" }
    [pscustomobject] $row
}

# The helper's own token (ADR 0006, "Defense in depth"). It drops every
# privilege tokenplan::HELPER_TOKEN doesn't keep when it starts, whatever the
# SCM gave it. Three trials, each a whole TUN run:
#   1. the SCM's default, as the MSI installs it (no list): the helper holds
#      exactly HELPER_PRIVILEGES;
#   2. the SCM gives only SeChangeNotifyPrivilege: data, whether the helper
#      also works without SeLoadDriverPrivilege (cleanup included), to
#      tighten HELPER_PRIVILEGES from;
#   3. the SCM gives every privilege SYSTEM holds: the helper still holds
#      exactly HELPER_PRIVILEGES. That list stays (it gives what no list
#      gives), and the steps after this one run with it.
# Trials 1 and 3 check what ships: they fail the step if TUN doesn't work or
# the helper's token isn't HELPER_PRIVILEGES. Trial 2 is data.
function Invoke-HelperTokenTrials([string[]] $SystemPrivileges) {
    $helperPrivileges = @(Read-PrivilegeList 'HELPER_PRIVILEGES')
    $rows = New-Object System.Collections.Generic.List[object]
    try {
        $rows.Add((Invoke-HelperTokenTrial 1 'its default (no list)' '' ''))
        $rows.Add((Invoke-HelperTokenTrial 2 'SeChangeNotifyPrivilege only' 'SeChangeNotifyPrivilege' 'SeChangeNotifyPrivilege'))
    } finally {
        # The SCM's list back to what no list gives, whatever happened.
        $rows.Add((Invoke-HelperTokenTrial 3 "every privilege SYSTEM holds ($(@($SystemPrivileges).Count))" (@($SystemPrivileges) -join '/') ''))
    }
    Stop-HelperService
    $table = $rows | Format-Table -AutoSize -Wrap | Out-String -Width 250
    $narrow = $rows[1]
    $shrinks = $narrow.'starts' -eq 'yes' -and $narrow.'TUN run' -eq 'works' -and
        $narrow.'its token' -eq 'as expected' -and $narrow.'sing-box' -eq 'as planned' -and
        $narrow.'cleanup' -eq 'none stale'
    $finding = if ($shrinks) {
        "finding: the helper works with SeChangeNotifyPrivilege alone (TUN, sing-box's token, the adapter cleanup): HELPER_PRIVILEGES ($($helperPrivileges -join ', ')) can shrink to it"
    } else {
        "finding: the helper does not fully work with SeChangeNotifyPrivilege alone (trial 2 above): HELPER_PRIVILEGES ($($helperPrivileges -join ', ')) stays"
    }
    Write-Host "---- the helper's own token, under what the SCM gives it:"
    Write-Host $table
    Write-Host $finding
    Set-Content -LiteralPath (Join-Path $LogDir 'helper-token-trials.txt') -Value ($table + $finding)
    foreach ($row in @($rows[0], $rows[2])) {
        $ok = $row.'starts' -eq 'yes' -and $row.'TUN run' -eq 'works' -and $row.'cleanup' -eq 'none stale' -and
            @('as expected', 'unreadable') -contains $row.'its token' -and
            @('as planned', 'unreadable') -contains $row.'sing-box'
        if (-not $ok) {
            throw "helper token trial $($row.'#') ($($row.'SCM gives')) failed: the helper as it ships doesn't hold its plan or doesn't run TUN (the table and its output are above)"
        }
    }
}

function Invoke-TokenProbeStep {
    $script:SmokeExe = Get-SmokeExe
    $probeExe = Get-ProbeExe
    # Nothing else brings TUN up meanwhile.
    Stop-HelperService
    $before = @(Get-SingTunDevices)
    if ($before.Count -gt 0) {
        Write-Note "sing-tun adapters exist before the token probe: $(($before | ForEach-Object { $_.FriendlyName }) -join ', ')"
    }
    if (Test-Path -LiteralPath $ProbeDir) {
        Remove-Item -LiteralPath $ProbeDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path $ProbeDir | Out-Null
    $acl = Invoke-Native 'icacls.exe' @($ProbeDir, '/inheritance:r', '/grant:r', '*S-1-5-18:(OI)(CI)F', '*S-1-5-32-544:(OI)(CI)F')
    if ($acl.ExitCode -ne 0) {
        throw "icacls $ProbeDir failed with $($acl.ExitCode): $($acl.Output)"
    }
    Copy-Item -LiteralPath $probeExe -Destination (Join-Path $ProbeDir 'token_probe.exe')

    $status = Invoke-ProbeAsSystem
    $summary = Join-Path $ProbeDir 'summary.txt'
    if (Test-Path -LiteralPath $summary) {
        Copy-Item -LiteralPath $summary -Destination (Join-Path $LogDir 'token-probe-summary.txt')
        Write-Host '---- the token probe''s summary'
        Get-Content -LiteralPath $summary | ForEach-Object { Write-Host $_ }
    }
    Copy-Item -LiteralPath (Join-Path $ProbeDir 'probe.log') -Destination (Join-Path $LogDir 'token-probe.log') -ErrorAction SilentlyContinue
    if ($status -ne 'ok') {
        throw "the token probe broke: $status"
    }
    $resultFile = Join-Path $ProbeDir 'result.txt'
    if (-not (Test-Path -LiteralPath $resultFile)) {
        throw "the token probe said ok but wrote no $resultFile"
    }
    $result = Get-Content -LiteralPath $resultFile -Raw | ConvertFrom-StringData
    # The regression check: the shipped plan installs wintun's driver and
    # carries traffic, and TUN needed no privilege NEVER_FOR_SING_BOX names.
    # Narrower tokens failing are data.
    if ($result['shipped_plan'] -ne 'works') {
        throw ("tokenplan::SING_BOX_TOKEN no longer runs TUN: on a first install $($result['shipped_install']), " +
            "in steady state $($result['shipped_steady']) (the probe's trials are above)")
    }
    if ($result['dangerous_needed'] -ne 'none') {
        throw "TUN needed privileges tokenplan::NEVER_FOR_SING_BOX names: $($result['dangerous_needed']) (the probe's trials are above)"
    }
    Write-Host 'ok: the shipped plan installs wintun''s driver and carries traffic; no dangerous privilege was needed'
    $system = @("$($result['system_privileges'])" -split '\s+' | Where-Object { $_ })
    if ($system.Count -eq 0) {
        throw "the token probe's result names no privilege SYSTEM holds"
    }
    Invoke-HelperTokenTrials $system
    Test-StaleAdapters
}

function Invoke-ProtocolStep {
    $script:SmokeExe = Get-SmokeExe
    $manifest = Get-Content -LiteralPath (Join-Path $HelperDir 'manifest.json') -Raw | ConvertFrom-Json
    $hash = (Get-FileHash -LiteralPath (Join-Path $HelperDir 'sing-box.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
    Invoke-Smoke 'hello', '--expect', 'start', '--sha256', $hash, '--sing-box-version', $manifest.sing_box.version
    Invoke-Smoke 'refused'
    Invoke-Smoke 'slots'
    Invoke-Smoke 'write-deadline'
    Invoke-ProbedTunRun
    Invoke-Smoke 'tun', '--end', 'close'
    Assert-TunDown
    Invoke-Smoke 'tun', '--end', 'mid-frame'
    Assert-TunDown
    Test-StaleAdapters
    Assert-StateTreePrivate
}

function Invoke-IdleExitStep {
    Write-Host 'waiting for the helper to stop by itself (60 s after its last connection)'
    $status = Wait-HelperStopped 80
    if ($status.Win32ExitCode -ne 0 -or $status.ServiceExitCode -ne $ExitCodes['OK']) {
        throw "the idle helper stopped with Win32 exit code $($status.Win32ExitCode), service-specific $($status.ServiceExitCode):`n$($status.Text)"
    }
    $said = @(Get-Content -LiteralPath $HelperLog -Tail 20 | Select-String -SimpleMatch 'idle for')
    if ($said.Count -eq 0) {
        throw "the helper stopped, but its log doesn't say it was idle"
    }
    Write-Host "ok: the helper stopped by itself with exit code 0: $($said[-1].Line)"
}

function Invoke-StandardUserStep {
    $script:SmokeExe = Get-SmokeExe
    # Stopped, so the first squat tries to create the name, not add to it.
    Stop-HelperService
    if (Test-Path -LiteralPath $WorkDir) {
        Remove-Item -LiteralPath $WorkDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path $WorkDir | Out-Null
    Copy-Item -LiteralPath $script:SmokeExe -Destination (Join-Path $WorkDir 'service_smoke.exe')
    $password = New-Password
    try {
        New-SmokeUser $password
        $grant = Invoke-Native 'icacls.exe' @($WorkDir, '/grant', "$env:COMPUTERNAME\${SmokeUser}:(OI)(CI)M")
        if ($grant.ExitCode -ne 0) {
            throw "icacls $WorkDir /grant failed with $($grant.ExitCode): $($grant.Output)"
        }
        $secure = ConvertTo-SecureString -String $password -AsPlainText -Force
        $password = $null
        $credential = New-Object System.Management.Automation.PSCredential -ArgumentList "$env:COMPUTERNAME\$SmokeUser", $secure

        Invoke-SmokeAsUser 'squat-stopped' 'squat --expect denied' $credential
        # Starts the service on demand, as the standard account.
        Invoke-SmokeAsUser 'hello' 'hello --expect readonly' $credential
        Invoke-SmokeAsUser 'squat-running' 'squat --expect denied' $credential
        Invoke-SmokeAsUser 'generic-write' 'generic-write --expect denied' $credential
        Invoke-SmokeAsUser 'unauthorized' 'unauthorized' $credential
        Assert-ReadOnlySlots $credential
        Assert-UserServiceRights $credential
        Assert-UserStateAccess $credential
    } finally {
        Remove-SmokeUser
    }
}

# After a break is undone: the helper runs again (an administrator's hello
# starts it), and `sc stop` stops it cleanly, with exit code 0, so the next
# break starts from a stopped service whose last exit code is 0.
function Assert-HelperRecovered {
    Invoke-Smoke 'hello', '--expect', 'start'
    Stop-HelperService
    $status = Get-HelperService
    if ($status.Win32ExitCode -ne 0 -or $status.ServiceExitCode -ne $ExitCodes['OK']) {
        throw "sc.exe stop: the helper stopped with Win32 exit code $($status.Win32ExitCode), service-specific $($status.ServiceExitCode):`n$($status.Text)"
    }
    Write-Host 'ok: the helper runs again, and sc.exe stop stops it with exit code 0'
}

function Invoke-BrokenInstallStep {
    $script:SmokeExe = Get-SmokeExe
    $singBox = Join-Path $HelperDir 'sing-box.exe'

    # A sing-box.exe that isn't the one the manifest hashes: one byte more.
    Stop-HelperService
    $hash = (Get-FileHash -LiteralPath $singBox -Algorithm SHA256).Hash
    $length = (Get-Item -LiteralPath $singBox).Length
    $stream = [System.IO.File]::Open($singBox, 'Append', 'Write')
    try {
        $stream.WriteByte(0)
    } finally {
        $stream.Dispose()
    }
    try {
        Assert-StartRefused 'a sing-box.exe that differs from the manifest' 'MANIFEST_REFUSED'
    } finally {
        $stream = [System.IO.File]::Open($singBox, 'Open', 'Write')
        try {
            $stream.SetLength($length)
        } finally {
            $stream.Dispose()
        }
        if ((Get-FileHash -LiteralPath $singBox -Algorithm SHA256).Hash -ne $hash) {
            throw "restoring $singBox failed: its hash differs from before"
        }
        Write-Host "restored $singBox"
    }
    Assert-HelperRecovered

    # A non-administrator who may write in the helper directory could plant
    # a DLL beside sing-box.
    Invoke-WithUsersAce $HelperDir 'Write' {
        Assert-StartRefused 'a Users write ACE on the helper directory' 'HELPER_DIR_REFUSED'
    }
    Assert-HelperRecovered

    # The same on sing-box.exe itself: the helper directory's code too ("or
    # a file in it"), not the manifest's, though the helper checks it as
    # it hashes it (WinSupervisor::open_binaries).
    Invoke-WithUsersAce $singBox 'Write' {
        Assert-StartRefused 'a Users write ACE on sing-box.exe' 'HELPER_DIR_REFUSED'
    }
    Assert-HelperRecovered

    # HelperState holds every account's cache and Tailscale keys: a Users
    # read is enough to refuse it.
    Invoke-WithUsersAce $StateDir 'ReadAndExecute' {
        Assert-StartRefused 'a Users read ACE on HelperState' 'STATE_DIR_REFUSED'
    }
    Assert-HelperRecovered

    # The pipe name held by another (administrator's) process.
    $ready = Join-Path $LogDir 'squat.ready'
    $release = Join-Path $LogDir 'squat.release'
    Remove-Files @($ready, $release)
    $background = Start-SmokeBackground 'squat-hold' @('squat', '--hold', '--ready-file', $ready, '--release-file', $release)
    try {
        Wait-SmokeReady $background $ready | Out-Null
        Assert-StartRefused 'the pipe name held by another process' 'PIPE_SQUATTED'
        $said = @(Get-Content -LiteralPath $HelperLog -Tail 20 | Select-String -SimpleMatch 'another process holds')
        if ($said.Count -eq 0) {
            throw "the helper stopped, but its log doesn't say the pipe was taken"
        }
    } finally {
        Complete-SmokeBackground $background $release
    }
    Assert-SmokeSucceeded $background
    Invoke-Smoke 'hello', '--expect', 'start'
}

function Invoke-KillHelperStep {
    $script:SmokeExe = Get-SmokeExe
    $ready = Join-Path $LogDir 'kill.ready'
    Remove-Files @($ready)
    $background = Start-SmokeBackground 'tun-helper-killed' @('tun', '--ready-file', $ready, '--end', 'helper-killed')
    try {
        Wait-SmokeReady $background $ready | Out-Null
        $helperPid = Get-HelperPid
        $singBox = Assert-SingBoxUnderHelper $helperPid
        Write-Host "killing the helper (pid $helperPid) while its sing-box (pid $($singBox.ProcessId)) runs"
        $kill = Invoke-Native 'taskkill.exe' @('/F', '/PID', "$helperPid")
        Write-Host $kill.Output
        if ($kill.ExitCode -ne 0) {
            Write-Host "taskkill failed with $($kill.ExitCode); trying Stop-Process"
            Stop-Process -Id $helperPid -Force
        }
        Assert-ProcessGone $helperPid 'the helper' 10
        Assert-ProcessGone $singBox.ProcessId "the helper's sing-box" 10
    } finally {
        Complete-SmokeBackground $background '' 90
    }
    Assert-SmokeSucceeded $background
    $status = Wait-HelperStopped 30
    Write-Host "after the kill the service is $($status.State), Win32 exit code $($status.Win32ExitCode)"
    # The helper died with its run, so the run directory waits for its next
    # start, which clears it (and the adapter, if one is left).
    Assert-TunDown -AllowRunDirs
    Invoke-Smoke 'hello', '--expect', 'start'
    $runs = @(Get-RunDirs)
    if ($runs.Count -ne 0) {
        throw "after the helper started again, HelperState\runs still holds $($runs.Count) entries"
    }
    Test-StaleAdapters
}

function Invoke-UninstallStep {
    $msiPath = Get-MsiPath
    $log = Join-Path $LogDir 'msi-uninstall.log'
    Write-Host "uninstalling $msiPath (log: $log)"
    $process = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\msiexec.exe') `
        -ArgumentList "/x `"$msiPath`" /qn /norestart /l*v `"$log`"" -Wait -PassThru
    if (@(0, 3010) -notcontains $process.ExitCode) {
        Show-MsiLog $log
        throw "msiexec /x exited with $($process.ExitCode) (the log's errors are above)"
    }
    # 1072: marked for deletion, until the last handle to it closes.
    $deadline = (Get-Date).AddSeconds(30)
    while ($true) {
        $query = Invoke-Sc query $ServiceName
        if ($query.ExitCode -eq 1060) {
            break
        }
        if ((Get-Date) -gt $deadline) {
            throw "after the uninstall, sc.exe query $ServiceName exits with $($query.ExitCode), not 1060 (no such service):`n$($query.Output)"
        }
        Start-Sleep -Milliseconds 500
    }
    if (Test-Path -LiteralPath $HelperDir) {
        $left = @(Get-ChildItem -LiteralPath $HelperDir -Force | ForEach-Object { $_.Name }) -join ', '
        throw "after the uninstall $HelperDir is still there: $left"
    }
    if (-not (Test-Path -LiteralPath $StateDir)) {
        throw 'the uninstall removed HelperState; once used it stays (ADR 0006 rule 7)'
    }
    Write-Host 'ok: the service is gone (1060), the helper directory too; HelperState stays'
}

# An MSI log's failures (each `Return value 3` with what led to it) and its
# end.
function Show-MsiLog([string] $Path) {
    if (-not (Test-Path -LiteralPath $Path)) {
        Write-Host "---- $Path doesn't exist"
        return
    }
    Write-Host "---- $Path, around each failure"
    Select-String -LiteralPath $Path -Pattern 'Return value 3' -Context 40, 0 | ForEach-Object { Write-Host $_ }
    Write-Host "---- $Path, its last 150 lines"
    Get-Content -LiteralPath $Path -Tail 150 | ForEach-Object { Write-Host $_ }
}

function Invoke-LogsStep {
    foreach ($log in @("$HelperLog.1", $HelperLog)) {
        if (Test-Path -LiteralPath $log) {
            Write-Host "==== $log"
            Get-Content -LiteralPath $log | ForEach-Object { Write-Host $_ }
        } else {
            Write-Host "==== $log doesn't exist"
        }
    }
    foreach ($log in @(Get-ChildItem -LiteralPath $LogDir -Filter 'msi-*.log' -ErrorAction SilentlyContinue)) {
        Write-Host "==== $($log.FullName)"
        Show-MsiLog $log.FullName
    }
    foreach ($directory in @($LogDir, $WorkDir)) {
        foreach ($file in @(Get-ChildItem -LiteralPath $directory -Filter '*.txt' -ErrorAction SilentlyContinue)) {
            Write-Host "==== $($file.FullName)"
            Get-Content -LiteralPath $file.FullName | ForEach-Object { Write-Host $_ }
        }
    }
}

$Steps = [ordered]@{
    'install'        = { Invoke-InstallStep }
    'inspect'        = { Invoke-InspectStep }
    'token-probe'    = { Invoke-TokenProbeStep }
    'protocol'       = { Invoke-ProtocolStep }
    'idle-exit'      = { Invoke-IdleExitStep }
    'standard-user'  = { Invoke-StandardUserStep }
    'broken-install' = { Invoke-BrokenInstallStep }
    'kill-helper'    = { Invoke-KillHelperStep }
    'uninstall'      = { Invoke-UninstallStep }
    'logs'           = { Invoke-LogsStep }
}

function Invoke-Step([string] $Name) {
    Write-Host "==== helper smoke: $Name"
    $began = Get-Date
    & $Steps[$Name]
    Write-Host ("==== helper smoke: $Name held ({0:N0} s)" -f ((Get-Date) - $began).TotalSeconds)
}

$script:SmokeExe = $null
try {
    if ($Step -eq 'all') {
        try {
            foreach ($name in @($Steps.Keys | Where-Object { $_ -ne 'logs' })) {
                Invoke-Step $name
            }
        } finally {
            Invoke-Step 'logs'
        }
    } else {
        Invoke-Step $Step
    }
} catch {
    $message = $_.Exception.Message
    if ($env:GITHUB_ACTIONS -eq 'true') {
        Write-Host "::error title=helper smoke ($Step)::$(($message -split "`n")[0])"
    }
    Write-Host "helper smoke ${Step}: FAILED: $message"
    Write-Host $_.ScriptStackTrace
    exit 1
}
exit 0
