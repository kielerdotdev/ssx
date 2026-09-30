<#
.SYNOPSIS
    Installs (or removes) ssx from an unpacked release archive. No administrator rights needed.

.DESCRIPTION
    Copies the programs to %LOCALAPPDATA%\Programs\ssx and adds that folder to your *user* PATH.
    With -Integrate it also enables start-at-login and the Explorer right-click entries.
    -Uninstall reverses all of it. Settings and history (%APPDATA%\ssx, %LOCALAPPDATA%\ssx) are
    never touched.

.EXAMPLE
    .\install.ps1 -Integrate
.EXAMPLE
    .\install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'Programs\ssx'),
    [switch]$Integrate,
    [switch]$Uninstall,
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$names = 'ssx', 'ssx-app', 'ssx-overlay', 'ssx-editor-ui', 'ssx-settings-ui'

function Invoke-Step([string]$What, [scriptblock]$Action) {
    if ($DryRun) { Write-Host "would: $What" } else { Write-Host $What; & $Action }
}

function Get-UserPath { [Environment]::GetEnvironmentVariable('Path', 'User') }

function Set-UserPath([string]$Value) {
    [Environment]::SetEnvironmentVariable('Path', $Value, 'User')
}

function Split-PathList([string]$List) {
    if ([string]::IsNullOrEmpty($List)) { return @() }
    $List.Split(';') | Where-Object { $_ -ne '' }
}

$ssx = Join-Path $Prefix 'ssx.exe'

if ($Uninstall) {
    if (Test-Path $ssx) {
        # Each of these only removes what ssx itself created; none may stop the file removal.
        foreach ($cmd in @('daemon stop', 'daemon autostart disable', 'shell uninstall', 'hotkeys uninstall')) {
            # ($cmd is not named $args: that is a reserved automatic variable in PowerShell.)
            Invoke-Step "ssx $cmd" { try { & $ssx @($cmd -split ' ') } catch { Write-Warning $_ } }
        }
    }
    Invoke-Step "remove the programs from $Prefix" {
        foreach ($n in $names) { Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $Prefix "$n.exe") }
        Remove-Item -Force -ErrorAction SilentlyContinue $Prefix
    }
    Invoke-Step "remove $Prefix from your user PATH" {
        $kept = Split-PathList (Get-UserPath) | Where-Object { $_.TrimEnd('\') -ne $Prefix.TrimEnd('\') }
        Set-UserPath ($kept -join ';')
    }
    Write-Host 'Removed ssx. Your settings and history were left in place.'
    # The best-effort `ssx ...` calls above may have left a non-zero $LASTEXITCODE behind, and a
    # PowerShell script exits with the last native command's code unless told otherwise.
    exit 0
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$bin = Join-Path $here 'bin'
if (-not (Test-Path $bin)) { throw "$bin not found; run this from the unpacked archive." }

Write-Host "Installing ssx into $Prefix"
Invoke-Step "create $Prefix" { New-Item -ItemType Directory -Force -Path $Prefix | Out-Null }
foreach ($n in $names) {
    $src = Join-Path $bin "$n.exe"
    if (-not (Test-Path $src)) { throw "$src is missing from the archive" }
    # A running ssx-app keeps its .exe locked; ask it to exit first so the copy can succeed.
    if (($n -eq 'ssx-app') -and (Test-Path $ssx) -and -not $DryRun) {
        try { & $ssx daemon stop 2>$null | Out-Null } catch { }
    }
    Invoke-Step "copy $n.exe" { Copy-Item -Force $src (Join-Path $Prefix "$n.exe") }
}

$onPath = Split-PathList (Get-UserPath) | Where-Object { $_.TrimEnd('\') -eq $Prefix.TrimEnd('\') }
if (-not $onPath) {
    Invoke-Step "add $Prefix to your user PATH (open a new terminal afterwards)" {
        $current = Get-UserPath
        Set-UserPath ($(if ($current) { "$current;$Prefix" } else { $Prefix }))
    }
}

if ($Integrate) {
    Invoke-Step 'enable start at login' { & $ssx daemon autostart enable }
    Invoke-Step 'add the right-click entries' { & $ssx shell install }
}

$global:LASTEXITCODE = 0

Write-Host @"

Installed. Next steps (in a new terminal):
  ssx doctor               what was detected on this machine
  ssx daemon start         tray icon, hotkeys, batching right-click uploads
  ssx-settings-ui          settings, workflows, uploaders, history
Remove everything with .\install.ps1 -Uninstall
"@
