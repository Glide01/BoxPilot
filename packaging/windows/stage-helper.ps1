<#
Stage BoxPilot's privileged helper for the MSI (ADR 0006 rules 3 and 7).

  packaging/windows/stage-helper.ps1 -Helper <boxpilot-helper.exe> `
      -SingBox <sing-box.exe> -SingBoxVersion <version> -OutDir <dir>

e.g. packaging/windows/stage-helper.ps1 `
       -Helper target/x86_64-pc-windows-msvc/release/boxpilot-helper.exe `
       -SingBox singbox-extract/sing-box-1.14.2-windows-amd64/sing-box.exe `
       -SingBoxVersion 1.14.2 `
       -OutDir target/x86_64-pc-windows-msvc/release/helper

-SingBox is sing-box.exe where its release archive was extracted: the files
beside it there are the ones it loads from beside itself. <OutDir> is
emptied, then holds exactly what wix/main.wxs installs into the fixed
[ProgramFiles64Folder]BoxPilot\Helper:

  boxpilot-helper.exe   the helper, which the BoxPilotHelper service runs
  sing-box.exe          the helper's own copy of sing-box
  libcronet.dll         only when the archive ships one beside sing-box.exe
                        (the naive outbound loads it from there); build the
                        MSI with -dHelperLibcronet=yes then, =no otherwise
  manifest.json         the install manifest the helper checks on every
                        start and spawn: sing-box's version and the SHA-256
                        of sing-box.exe and of libcronet.dll, as staged here

manifest.json is in the format crates/boxpilot-helper/src/manifest.rs
parses, which is strict: UTF-8 without a BOM, objects with exactly the
known fields, 64 lowercase hex digits per hash. The helper refuses to run
on anything else. Check a staged directory with the helper's own parser:

  BOXPILOT_HELPER_STAGE=<absolute OutDir> cargo test -p boxpilot-helper `
      --test install_manifest -- --ignored

Any file beside sing-box.exe other than libcronet.dll and LICENSE stops
the script: sing-box might load it, so it would have to be staged and
hashed too (here, in the manifest and in wix/main.wxs) before it ships.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $Helper,
    [Parameter(Mandatory = $true)] [string] $SingBox,
    [Parameter(Mandatory = $true)] [string] $SingBoxVersion,
    [Parameter(Mandatory = $true)] [string] $OutDir
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

# What sing-box's Windows archive may hold beside sing-box.exe.
$Libcronet = 'libcronet.dll'
$ArchiveFiles = @('sing-box.exe', $Libcronet, 'LICENSE')

function Get-Sha256([string] $Path) {
    $hash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($hash -cnotmatch '^[0-9a-f]{64}\z') {
        throw "stage-helper: unexpected SHA-256 '$hash' for $Path"
    }
    $hash
}

# The manifest's version rule (manifest.rs) and the release workflow's.
if ($SingBoxVersion.Length -gt 64 -or
    $SingBoxVersion -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?\z') {
    throw "stage-helper: '$SingBoxVersion' is not a sing-box version (expected e.g. 1.14.2)"
}
if (-not (Test-Path -LiteralPath $Helper -PathType Leaf)) {
    throw "stage-helper: the helper is not at $Helper"
}
if (-not (Test-Path -LiteralPath $SingBox -PathType Leaf)) {
    throw "stage-helper: sing-box is not at $SingBox"
}
$singBoxFile = Get-Item -LiteralPath $SingBox
if ($singBoxFile.Name -ne 'sing-box.exe') {
    throw "stage-helper: -SingBox must name sing-box.exe, not $($singBoxFile.Name)"
}
$archiveDir = $singBoxFile.Directory.FullName
foreach ($entry in @(Get-ChildItem -LiteralPath $archiveDir -Force)) {
    if ($ArchiveFiles -notcontains $entry.Name) {
        throw ("stage-helper: sing-box's archive has '$($entry.Name)' beside sing-box.exe. " +
            "If sing-box loads it, stage and hash it like $Libcronet (this script, " +
            "wix/main.wxs); either way, add it to the list at the top of this script.")
    }
}

if (Test-Path -LiteralPath $OutDir) {
    Remove-Item -LiteralPath $OutDir -Recurse -Force
}
$out = (New-Item -ItemType Directory -Path $OutDir).FullName

Copy-Item -LiteralPath $Helper -Destination (Join-Path $out 'boxpilot-helper.exe')
Copy-Item -LiteralPath $singBoxFile.FullName -Destination (Join-Path $out 'sing-box.exe')
$extraFiles = @()
$cronet = Join-Path $archiveDir $Libcronet
if (Test-Path -LiteralPath $cronet -PathType Leaf) {
    Copy-Item -LiteralPath $cronet -Destination (Join-Path $out $Libcronet)
    $extraFiles += $Libcronet
}

# Hashed from the staged copies: these are the bytes the MSI installs.
# Written by hand rather than with ConvertTo-Json, whose output differs
# between PowerShell versions; every value is checked above, so none needs
# escaping.
$extraEntries = @(foreach ($name in $extraFiles) {
        '    {{"file": "{0}", "sha256": "{1}"}}' -f $name, (Get-Sha256 (Join-Path $out $name))
    })
if ($extraEntries.Count -gt 0) {
    $extraJson = "[`n" + ($extraEntries -join ",`n") + "`n  ]"
} else {
    $extraJson = '[]'
}
$manifest = @(
    '{',
    '  "manifest_version": 1,',
    '  "sing_box": {',
    '    "file": "sing-box.exe",',
    ('    "version": "{0}",' -f $SingBoxVersion),
    ('    "sha256": "{0}"' -f (Get-Sha256 (Join-Path $out 'sing-box.exe'))),
    '  },',
    ('  "extra_files": {0}' -f $extraJson),
    '}',
    ''
) -join "`n"
$manifestPath = Join-Path $out 'manifest.json'
[System.IO.File]::WriteAllText($manifestPath, $manifest, (New-Object System.Text.UTF8Encoding $false))

Write-Host "Staged the privileged helper in ${out}:"
Get-ChildItem -LiteralPath $out | ForEach-Object { Write-Host "  $($_.Name)" }
Write-Host $manifest
