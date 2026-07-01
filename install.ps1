#Requires -Version 5.1
<#
.SYNOPSIS
    noaa-recon-api installer / updater / uninstaller for Windows (local testing).

.DESCRIPTION
    Windows counterpart to install.sh. Scoped for local testing rather than
    production deployment: no reverse proxy / domain / HTTPS / firewall
    steps (see install.sh + INSTALL.md for that side) -- just Python, git,
    a virtualenv, the storm/recon archives, and a `noaa-recon-api` command
    that starts/stops the API as a plain background process (no Windows
    Service, no autostart-on-login -- start it when you want to test it).

.EXAMPLE
    irm https://raw.githubusercontent.com/jjmurdock19/noaa-recon-api/main/install.ps1 | iex

.EXAMPLE
    # Read it first, then run it
    irm https://raw.githubusercontent.com/jjmurdock19/noaa-recon-api/main/install.ps1 -OutFile install.ps1
    notepad install.ps1
    .\install.ps1
#>
param(
    [switch]$Update,
    [switch]$Uninstall,
    [switch]$Status,
    [string]$Dir,
    [string]$Branch = "main",
    [switch]$Yes,
    [switch]$Help
)

$ErrorActionPreference = "Stop"

$RepoUrl = "https://github.com/jjmurdock19/noaa-recon-api.git"
$DefaultInstallDir = Join-Path $env:LOCALAPPDATA "noaa-recon-api"
$ConfigFileName = "install.conf.json"

# ---------------------------------------------------------------------------
# UI helpers
# ---------------------------------------------------------------------------
function Write-Step  ($msg) { Write-Host ""; Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok    ($msg) { Write-Host "  ok  $msg" -ForegroundColor Green }
function Write-Warn2 ($msg) { Write-Host "  !!  $msg" -ForegroundColor Yellow }
function Write-Err2  ($msg) { Write-Host "  xx  $msg" -ForegroundColor Red }
function Die         ($msg) { Write-Err2 $msg; exit 1 }

# $ErrorActionPreference = "Stop" only catches PowerShell's own terminating
# errors -- a native .exe (git, pip, python) returning a non-zero exit code
# does NOT throw on its own, so failures there would otherwise be silently
# ignored and the install would limp forward "successfully" with a broken
# venv/repo. Call this right after any native command whose failure should
# actually stop the install.
function Invoke-Checked($Description) {
    if ($LASTEXITCODE -ne 0) {
        Die "$Description failed (exit code $LASTEXITCODE) -- see the output above for details."
    }
}

function Test-Interactive {
    return (-not $Yes) -and [Environment]::UserInteractive -and (-not [Console]::IsInputRedirected)
}

function Ask-Text($Prompt, $Default) {
    if (-not (Test-Interactive)) { return $Default }
    $suffix = if ($Default) { " [$Default]" } else { "" }
    $ans = Read-Host "  $Prompt$suffix"
    if ([string]::IsNullOrWhiteSpace($ans)) { return $Default }
    return $ans
}

function Ask-YesNo($Prompt, $Default = 'y') {
    if (-not (Test-Interactive)) { return $Default -eq 'y' }
    $hint = if ($Default -eq 'y') { 'Y/n' } else { 'y/N' }
    while ($true) {
        $ans = Read-Host "  $Prompt [$hint]"
        if ([string]::IsNullOrWhiteSpace($ans)) { $ans = $Default }
        if ($ans -match '^[Yy]') { return $true }
        if ($ans -match '^[Nn]') { return $false }
        Write-Host "  please answer y or n"
    }
}

# Returns a 0-based index. Not an arrow-key menu (that's install.sh's thing) --
# plain numbered choices are the standard, reliable idiom for a PowerShell
# installer and need no raw-console-mode handling to get right.
function Ask-Choice($Prompt, [string[]]$Options) {
    if (-not (Test-Interactive)) { return 0 }
    Write-Host ""
    Write-Host "  $Prompt" -ForegroundColor White
    for ($i = 0; $i -lt $Options.Count; $i++) { Write-Host "    [$($i + 1)] $($Options[$i])" }
    while ($true) {
        $ans = Read-Host "  Choice [1]"
        if ([string]::IsNullOrWhiteSpace($ans)) { return 0 }
        if ($ans -match '^\d+$' -and [int]$ans -ge 1 -and [int]$ans -le $Options.Count) { return [int]$ans - 1 }
        Write-Host "  enter a number between 1 and $($Options.Count)"
    }
}

function Print-Banner {
    Write-Host ""
    Write-Host "  noaa-recon-api" -ForegroundColor Cyan -NoNewline
    Write-Host " installer (Windows / local testing)"
    Write-Host "  Open-source API for archival GOES satellite imagery, NOAA Tail" -ForegroundColor DarkGray
    Write-Host "  Doppler Radar, and hurricane hunter recon data." -ForegroundColor DarkGray
    Write-Host "  github.com/jjmurdock19/noaa-recon-api" -ForegroundColor DarkGray
    Write-Host ""
}

function Print-Help {
    @"
noaa-recon-api installer (Windows)

Usage:
  .\install.ps1                  Interactive install / update / reconfigure wizard
  .\install.ps1 -Update          Non-interactive: pull latest code, reinstall deps, restart
  .\install.ps1 -Uninstall       Stop the API and remove the noaa-recon-api command
  .\install.ps1 -Status          Show whether it's running and a health check
  .\install.ps1 -Dir PATH        Install to PATH instead of %LOCALAPPDATA%\noaa-recon-api
  .\install.ps1 -Branch NAME     Track a branch other than 'main'
  .\install.ps1 -Yes             Accept defaults for anything not given on the command line

This installer is scoped for LOCAL TESTING: no reverse proxy, domain, or
HTTPS setup, and the API runs as a plain background process you start and
stop yourself (`noaa-recon-api start` / `stop`) -- it does not register a
Windows Service or start automatically at login. For a persistent
production deployment, use install.sh on Linux instead.
"@
}

# ---------------------------------------------------------------------------
# Prerequisite checks
# ---------------------------------------------------------------------------
function Test-CommandExists($Name) {
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

function Test-Winget {
    return Test-CommandExists "winget"
}

# winget updates the registry's Machine/User PATH, but this *process's*
# $env:Path was captured at launch and won't see that change on its own --
# re-derive it so a freshly winget-installed git/python is usable without
# forcing the user to close and reopen the terminal.
function Update-SessionPath {
    $machine = [System.Environment]::GetEnvironmentVariable("Path", "Machine")
    $user = [System.Environment]::GetEnvironmentVariable("Path", "User")
    $env:Path = @($machine, $user) -join ';'
}

function Ensure-Git {
    if (Test-CommandExists "git") { Write-Ok "git found"; return }
    Write-Warn2 "git not found."
    if ((Test-Winget) -and (Ask-YesNo "Install git via winget?" 'y')) {
        winget install --id Git.Git -e --accept-package-agreements --accept-source-agreements
        Update-SessionPath
        if (Test-CommandExists "git") { Write-Ok "git installed"; return }
        Die "git installed, but this terminal session can't see it yet -- open a NEW PowerShell window and re-run this installer."
    }
    Die "Install git manually from https://git-scm.com/download/win, then re-run this script."
}

# Only the *initial* venv creation needs to hunt for a system Python launcher --
# every step after that uses the venv's own python.exe directly, so this is
# the only place launcher differences (py / python3 / python) matter.
function Get-PythonCmd {
    $candidates = @(
        @{ Exe = "py"; Args = @("-3") },
        @{ Exe = "python3"; Args = @() },
        @{ Exe = "python"; Args = @() }
    )
    foreach ($c in $candidates) {
        if (-not (Test-CommandExists $c.Exe)) { continue }
        try {
            # 2>&1 can yield an array of lines; array `-match` filters instead of
            # populating $Matches the way scalar `-match` does, so join first.
            $verOut = (& $c.Exe @($c.Args + "--version") 2>&1) -join "`n"
            if ($verOut -match '(\d+)\.(\d+)') {
                $maj = [int]$Matches[1]; $min = [int]$Matches[2]
                if ($maj -gt 3 -or ($maj -eq 3 -and $min -ge 9)) { return $c }
            }
        } catch {}
    }
    return $null
}

function Ensure-Python {
    $cmd = Get-PythonCmd
    if ($cmd) { Write-Ok "python found ($($cmd.Exe) $($cmd.Args -join ' '))"; return $cmd }
    Write-Warn2 "Python 3.9+ not found."
    if ((Test-Winget) -and (Ask-YesNo "Install Python via winget?" 'y')) {
        winget install --id Python.Python.3.12 -e --accept-package-agreements --accept-source-agreements
        Update-SessionPath
        $cmd = Get-PythonCmd
        if ($cmd) { Write-Ok "python installed ($($cmd.Exe) $($cmd.Args -join ' '))"; return $cmd }
        Die "Python installed, but this terminal session can't see it yet -- open a NEW PowerShell window and re-run this installer."
    }
    Die "Install Python 3.9+ manually from https://www.python.org/downloads/ (check 'Add python.exe to PATH' during setup), then re-run this script."
}

# ---------------------------------------------------------------------------
# Config persistence (inside the install dir -- no /etc-style shared
# location on Windows, and no permission games to get wrong: it's the
# invoking user's own directory the whole way through)
# ---------------------------------------------------------------------------
function Save-Config {
    $cfgPath = Join-Path $InstallDir $ConfigFileName
    @{
        InstallDir = $InstallDir
        BindHost   = $BindHost
        Port       = $Port
        Branch     = $Branch
    } | ConvertTo-Json | Set-Content -Path $cfgPath -Encoding UTF8
}

function Load-Config($path) {
    if (-not (Test-Path $path)) { return $null }
    try { return (Get-Content $path -Raw | ConvertFrom-Json) } catch { return $null }
}

# ---------------------------------------------------------------------------
# Wizard steps
# ---------------------------------------------------------------------------
function Sync-Repo {
    if (Test-Path (Join-Path $InstallDir ".git")) {
        Write-Step "Existing repo at $InstallDir -- syncing to origin/$Branch"
        Push-Location $InstallDir
        try {
            git fetch origin;                        Invoke-Checked "git fetch"
            git reset --hard "origin/$Branch";        Invoke-Checked "git reset"
            git submodule update --init --recursive;  Invoke-Checked "git submodule update"
        } finally { Pop-Location }
    } else {
        Write-Step "Cloning $RepoUrl into $InstallDir"
        $parent = Split-Path $InstallDir -Parent
        if ($parent -and -not (Test-Path $parent)) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
        git clone --branch $Branch --recurse-submodules $RepoUrl $InstallDir
        Invoke-Checked "git clone"
    }
    $rev = (git -C $InstallDir rev-parse --short HEAD)
    Write-Ok "repo ready at $InstallDir ($rev)"
}

function Setup-Venv {
    Write-Step "Creating the Python virtual environment and installing dependencies (can take a minute)"
    $venvDir = Join-Path $InstallDir ".venv"
    if (-not (Test-Path (Join-Path $venvDir "Scripts\python.exe"))) {
        & $PythonCmd.Exe @($PythonCmd.Args + @("-m", "venv", $venvDir))
        Invoke-Checked "python -m venv"
    }
    $pip = Join-Path $venvDir "Scripts\pip.exe"
    & $pip install --upgrade pip -q
    Invoke-Checked "pip install --upgrade pip"
    & $pip install -e $InstallDir -q
    Invoke-Checked "pip install -e . (dependency install -- check for a Windows wheel/build failure above, netCDF4 and Pillow are the usual suspects)"
    Write-Ok "virtualenv ready"
}

function Setup-AdminCredentials {
    $credFile = Join-Path $InstallDir "admin_credentials.json"
    if (Test-Path $credFile) { Write-Ok "admin_credentials.json already exists -- leaving it alone"; return }
    Write-Step "Setting up the admin console login (cache/database management UI)"
    $venvPython = Join-Path $InstallDir ".venv\Scripts\python.exe"
    $user = Ask-Text "Admin console username" "admin"
    if (Ask-YesNo "Generate a random admin password (recommended)?" 'y') {
        $pass = & $venvPython -c "import secrets;print(secrets.token_urlsafe(16))"
    } else {
        $pass = Ask-Text "Admin console password" ""
    }
    $secret = & $venvPython -c "import secrets;print(secrets.token_hex(32))"
    @{ username = $user; password = $pass; secret_key = $secret } |
        ConvertTo-Json | Set-Content -Path $credFile -Encoding UTF8
    $script:AdminUser = $user
    $script:AdminPass = $pass
    Write-Ok "admin console credentials set (username: $user)"
}

function Choose-Network {
    $choice = Ask-Choice "How should the API be reachable?" @(
        "Just this machine (127.0.0.1 only -- recommended for local testing)",
        "My local network (any device on the LAN, no domain)"
    )
    $script:BindHost = if ($choice -eq 1) { "0.0.0.0" } else { "127.0.0.1" }
    $portDefault = if ($script:Port) { $script:Port } else { "8000" }
    $script:Port = Ask-Text "Port to run the API on" $portDefault
}

function Build-Archives {
    $venvPython = Join-Path $InstallDir ".venv\Scripts\python.exe"
    Write-Step "Building the storm-track archive (backs GET /v1/storms/*, usually ~10s)"
    & $venvPython (Join-Path $InstallDir "scripts\ingest_storms.py")
    Write-Ok "storm archive built"

    if (Ask-YesNo "Build the FULL recon MET archive now (every hurricane hunter mission since 2011)? This can take SEVERAL HOURS. Choosing no builds just current+previous season (fast, minutes)." 'n') {
        Write-Step "Building the full recon MET archive -- this will take a while"
        & $venvPython (Join-Path $InstallDir "scripts\ingest_recon_met.py") --full
    } else {
        Write-Step "Building the recon MET archive (current + previous season)"
        & $venvPython (Join-Path $InstallDir "scripts\ingest_recon_met.py")
    }
    Write-Ok "recon MET archive built"
}

# ---------------------------------------------------------------------------
# The `noaa-recon-api` command: a .cmd shim (so it's runnable from cmd.exe
# and PowerShell alike without changing anyone's PATHEXT) plus the .ps1 that
# actually does the work. start/stop/status/logs are handled right here;
# update/uninstall just re-invoke this installer, same as install.sh's CLI
# wrapper delegates to install.sh --update/--uninstall.
# ---------------------------------------------------------------------------
function Install-Cli {
    Write-Step "Installing the 'noaa-recon-api' command"
    $binDir = Join-Path $InstallDir "bin"
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null

    $ps1Path = Join-Path $binDir "noaa-recon-api.ps1"
    $ps1Content = @"
param([Parameter(Position=0)][string]`$Command)
`$InstallDir = "$InstallDir"
`$BindHostValue = "$BindHost"
`$Port = "$Port"
`$PidFile = Join-Path `$InstallDir "run.pid"
`$LogDir = Join-Path `$InstallDir "logs"

function Start-Api {
    if (Test-Path `$PidFile) {
        `$existingPid = Get-Content `$PidFile
        if (Get-Process -Id `$existingPid -ErrorAction SilentlyContinue) {
            Write-Host "Already running (PID `$existingPid)."
            return
        }
    }
    New-Item -ItemType Directory -Force -Path `$LogDir | Out-Null
    `$venvPython = Join-Path `$InstallDir ".venv\Scripts\python.exe"
    `$proc = Start-Process -FilePath `$venvPython ``
        -ArgumentList @("-m","uvicorn","app.main:app","--host",`$BindHostValue,"--port",`$Port) ``
        -WorkingDirectory `$InstallDir ``
        -WindowStyle Hidden ``
        -RedirectStandardOutput (Join-Path `$LogDir "uvicorn-out.log") ``
        -RedirectStandardError (Join-Path `$LogDir "uvicorn-err.log") ``
        -PassThru
    `$proc.Id | Set-Content `$PidFile

    # Wait up to ~15s for it to actually answer -- not just for the process to
    # still exist a moment later. A process can survive a couple seconds and
    # then die on import (a broken/incomplete venv install is the classic
    # cause) or just be slow importing numpy/netCDF4/Pillow on a cold start.
    `$healthy = `$false
    for (`$i = 0; `$i -lt 15; `$i++) {
        if (-not (Get-Process -Id `$proc.Id -ErrorAction SilentlyContinue)) { break }
        try {
            Invoke-RestMethod -Uri "http://127.0.0.1:`$Port/v1/health" -TimeoutSec 1 | Out-Null
            `$healthy = `$true
            break
        } catch { Start-Sleep -Seconds 1 }
    }

    if (`$healthy) {
        Write-Host "Started (PID `$(`$proc.Id)). API: http://`${BindHostValue}:`${Port}"
    } elseif (Get-Process -Id `$proc.Id -ErrorAction SilentlyContinue) {
        Write-Host "Process is running (PID `$(`$proc.Id)) but isn't answering http://127.0.0.1:`$Port/v1/health yet."
        Write-Host "Check `$LogDir\uvicorn-err.log -- if it's still starting, run 'noaa-recon-api status' again in a few seconds."
    } else {
        Write-Host "Failed to start -- check `$LogDir\uvicorn-err.log for the actual error."
        Remove-Item `$PidFile -Force -ErrorAction SilentlyContinue
    }
}

function Stop-Api {
    if (-not (Test-Path `$PidFile)) { Write-Host "Not running (no pid file)."; return }
    `$existingPid = Get-Content `$PidFile
    Stop-Process -Id `$existingPid -Force -ErrorAction SilentlyContinue
    Remove-Item `$PidFile -Force -ErrorAction SilentlyContinue
    Write-Host "Stopped."
}

function Status-Api {
    if ((Test-Path `$PidFile) -and (Get-Process -Id (Get-Content `$PidFile) -ErrorAction SilentlyContinue)) {
        Write-Host "Running (PID `$(Get-Content `$PidFile))."
    } else {
        Write-Host "Not running."
    }
    try {
        `$r = Invoke-RestMethod -Uri "http://127.0.0.1:`$Port/v1/health" -TimeoutSec 3
        Write-Host "Health: `$(`$r | ConvertTo-Json -Compress)"
    } catch {
        Write-Host "(health check failed -- is it running? try: noaa-recon-api start)"
    }
}

switch (`$Command) {
    "start"     { Start-Api }
    "stop"      { Stop-Api }
    "restart"   { Stop-Api; Start-Api }
    "status"    { Status-Api }
    "logs"      { Get-Content (Join-Path `$LogDir "uvicorn-out.log") -Wait -Tail 20 }
    "update"    { powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path `$InstallDir "install.ps1") -Update -Dir `$InstallDir }
    "uninstall" { powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path `$InstallDir "install.ps1") -Uninstall -Dir `$InstallDir }
    default     { Write-Host "Usage: noaa-recon-api {start|stop|restart|status|logs|update|uninstall}" }
}
"@
    Set-Content -Path $ps1Path -Value $ps1Content -Encoding UTF8

    $cmdPath = Join-Path $binDir "noaa-recon-api.cmd"
    Set-Content -Path $cmdPath -Value "@echo off`r`npowershell -NoProfile -ExecutionPolicy Bypass -File `"%~dp0noaa-recon-api.ps1`" %*" -Encoding ASCII

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$binDir*") {
        $newPath = if ($userPath) { "$userPath;$binDir" } else { $binDir }
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    }
    # Same fix as winget-installed git/python above: the registry write persists
    # for future sessions, but *this* session's $env:Path was already loaded --
    # refresh it too so `noaa-recon-api` works right away, no new window needed.
    Update-SessionPath
    Write-Ok "try: noaa-recon-api status"
}

function Print-Summary {
    $url = "http://${BindHost}:${Port}"
    $pidFile = Join-Path $InstallDir "run.pid"
    $running = (Test-Path $pidFile) -and (Get-Process -Id (Get-Content $pidFile) -ErrorAction SilentlyContinue)

    Write-Host ""
    Write-Host "noaa-recon-api is installed." -ForegroundColor Green
    Write-Host ""
    Write-Host "  API:    $url"
    Write-Host "  Docs:   $url/docs"
    Write-Host "  Admin:  $url/"
    if ($script:AdminUser) {
        Write-Host "  Login:  $($script:AdminUser) / $($script:AdminPass)   (save this -- shown once)" -ForegroundColor DarkGray
    }
    Write-Host ""
    if ($running) {
        Write-Host "  It's running now."
    } else {
        Write-Host "  It is not running -- start it with: noaa-recon-api start"
    }
    Write-Host "  It never auto-starts at login either way (local-testing scope, not a service)."
    Write-Host ""
    Write-Host "  Manage it (works right here -- also on PATH for new terminals from now on):"
    Write-Host "    noaa-recon-api start      -- start it in the background"
    Write-Host "    noaa-recon-api stop       -- stop it"
    Write-Host "    noaa-recon-api status     -- is it running?"
    Write-Host "    noaa-recon-api logs       -- tail the logs"
    Write-Host "    noaa-recon-api update     -- pull the latest from GitHub and restart"
    Write-Host "    noaa-recon-api uninstall  -- remove everything"
    Write-Host ""
    Write-Host "  Config: $(Join-Path $InstallDir $ConfigFileName)"
}

# ---------------------------------------------------------------------------
# Top-level commands
# ---------------------------------------------------------------------------
function Invoke-Install {
    Print-Banner

    $cfgPath = Join-Path $InstallDir $ConfigFileName
    $existing = Load-Config $cfgPath
    if ($existing) {
        Write-Step "Existing installation detected at $InstallDir"
        $choice = Ask-Choice "What would you like to do?" @(
            "Update to the latest version (git pull + restart)",
            "Reconfigure (re-run the setup wizard)",
            "Uninstall",
            "Cancel"
        )
        switch ($choice) {
            0 { Invoke-Update; return }
            2 { Invoke-Uninstall; return }
            3 { Write-Host "Cancelled."; return }
        }
        $script:BindHost = $existing.BindHost
        $script:Port = $existing.Port
    }

    Ensure-Git
    $script:PythonCmd = Ensure-Python
    $script:InstallDir = Ask-Text "Where should noaa-recon-api live?" $InstallDir

    Sync-Repo
    Setup-Venv
    Setup-AdminCredentials
    Choose-Network
    if (Ask-YesNo "Build the storm-track and recon MET archives now?" 'y') { Build-Archives }
    Install-Cli
    Save-Config

    if (Ask-YesNo "Start the API now?" 'y') {
        & (Join-Path $InstallDir "bin\noaa-recon-api.cmd") start
    }
    Print-Summary
}

function Invoke-Update {
    $cfgPath = Join-Path $InstallDir $ConfigFileName
    $existing = Load-Config $cfgPath
    if (-not $existing) { Die "No existing install found at $cfgPath. Run install.ps1 normally first." }
    $script:BindHost = $existing.BindHost
    $script:Port = $existing.Port

    Write-Step "Updating $InstallDir to the latest $Branch"
    Push-Location $InstallDir
    try {
        git fetch origin;                       Invoke-Checked "git fetch"
        git reset --hard "origin/$Branch";       Invoke-Checked "git reset"
        git submodule update --init --recursive; Invoke-Checked "git submodule update"
        & (Join-Path $InstallDir ".venv\Scripts\pip.exe") install -e $InstallDir -q
        Invoke-Checked "pip install -e ."
    } finally { Pop-Location }

    $binCmd = Join-Path $InstallDir "bin\noaa-recon-api.cmd"
    $wasRunning = Test-Path (Join-Path $InstallDir "run.pid")
    if ($wasRunning) { & $binCmd stop }
    Install-Cli   # regenerate the control script in case its own logic changed
    if ($wasRunning) { & $binCmd start }
    Write-Ok "updated -- now on $(git -C $InstallDir rev-parse --short HEAD)"
}

function Invoke-Uninstall {
    $cfgPath = Join-Path $InstallDir $ConfigFileName
    $existing = Load-Config $cfgPath
    if (-not $existing) { Die "Nothing to uninstall -- no config found at $cfgPath." }
    Write-Warn2 "This stops the API and removes the noaa-recon-api command."
    if (-not (Ask-YesNo "Continue?" 'n')) { Write-Host "Cancelled."; return }

    $binCmd = Join-Path $InstallDir "bin\noaa-recon-api.cmd"
    if (Test-Path $binCmd) { & $binCmd stop }

    $binDir = Join-Path $InstallDir "bin"
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -like "*$binDir*") {
        $newPath = ($userPath -split ';' | Where-Object { $_ -and ($_ -ne $binDir) }) -join ';'
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    }

    if (Ask-YesNo "Also delete the installed code and databases at $InstallDir? This deletes data\*.sqlite too and cannot be undone." 'n') {
        Remove-Item -Recurse -Force $InstallDir -ErrorAction SilentlyContinue
    } else {
        Remove-Item -Force $cfgPath -ErrorAction SilentlyContinue
    }
    Write-Ok "uninstalled"
}

# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------
if ($Help) { Print-Help; exit 0 }

$script:InstallDir = if ($Dir) { $Dir } else { $DefaultInstallDir }

if ($Uninstall) {
    Invoke-Uninstall
} elseif ($Update) {
    Invoke-Update
} elseif ($Status) {
    $binCmd = Join-Path $InstallDir "bin\noaa-recon-api.cmd"
    if (Test-Path $binCmd) { & $binCmd status } else { Die "No install found at $InstallDir." }
} else {
    Invoke-Install
}
