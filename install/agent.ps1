# winstats agent installer for Windows.
#
#   irm https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.ps1 | iex
#
# Downloads the newest agent from the GitHub releases into %LOCALAPPDATA%\Programs\winstats-agent,
# makes it start at logon, gives it a token and prints how to add the machine in winstats.
# Running it again updates the agent and keeps its settings and token.
#
# On a Windows machine that runs winstats itself no agent is needed: Remote machines > Share this
# machine does the same.
#
# Settings through the environment before the command:
#   $env:WINSTATS_TOKEN = '...'      use this token (default: a new install gets a random one, an
#                                    update keeps the configured one)
#   $env:WINSTATS_NAME = '...'       name shown in winstats (default: the computer name)
#   $env:WINSTATS_NO_TOKEN = '1'     no token: anyone on the network may read this machine
#   $env:WINSTATS_UNINSTALL = '1'    stop the agent and remove it (its config stays)
#   $env:WINSTATS_NO_FIREWALL = '1'  no Windows Firewall rules (they are added for an administrator)
#   $env:WINSTATS_VERSION = 'v0.3.4' a release instead of the newest
#   $env:WINSTATS_BASE = '...'       where the files are (tests: a URL or a folder)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest is many times slower with the bar

$Repo = 'mgvs/winstats'
$Port = 47778
$BeaconPort = 47777

function Say($text) { Write-Host "==> $text" -ForegroundColor Cyan }
function Warn($text) { Write-Host "warning: $text" -ForegroundColor Yellow }
function Die($text) { Write-Host "error: $text" -ForegroundColor Red; throw $text }

function Install-WinstatsAgent {
    $token = $env:WINSTATS_TOKEN
    $name = $env:WINSTATS_NAME
    if ($env:WINSTATS_BASE) { $base = $env:WINSTATS_BASE }
    elseif ($env:WINSTATS_VERSION) { $base = "https://github.com/$Repo/releases/download/$($env:WINSTATS_VERSION)" }
    else { $base = "https://github.com/$Repo/releases/latest/download" }

    # the machine's architecture, also from a 32-bit PowerShell on 64-bit Windows
    $machine = $env:PROCESSOR_ARCHITEW6432
    if (-not $machine) { $machine = $env:PROCESSOR_ARCHITECTURE }
    switch ($machine) {
        'AMD64' { $arch = 'x86_64' }
        'ARM64' { $arch = 'arm64' }
        'x86' { $arch = 'x86' }
        default { Die "no agent build for $machine" }
    }

    $dir = Join-Path $env:LOCALAPPDATA 'Programs\winstats-agent'
    $exe = Join-Path $dir 'winstats-agent.exe'
    $conf = Join-Path $env:APPDATA 'winstats-agent\agent.toml'

    if ($env:WINSTATS_UNINSTALL -eq '1') {
        if (Test-Path $exe) { & $exe uninstall-service | Out-Null }
        $running = @(Get-Process winstats-agent -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })
        $running | Stop-Process -Force
        $running | Wait-Process -Timeout 5 -ErrorAction SilentlyContinue
        Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
        Get-NetFirewallRule -DisplayName 'winstats agent*' -ErrorAction SilentlyContinue | Remove-NetFirewallRule -ErrorAction SilentlyContinue
        Say "removed; the config stays in $conf"
        return
    }

    $file = "winstats-agent-windows-$arch.exe"
    $tmp = Join-Path ([IO.Path]::GetTempPath()) "winstats-agent-$PID.exe"
    Say "downloading $file"
    if ($base -match '^[a-z]+://') {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$file" -OutFile $tmp
    } else {
        Copy-Item (Join-Path $base $file) $tmp
    }

    # the running agent holds its exe: stop it, then swap the file
    $running = @(Get-Process winstats-agent -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })
    $running | Stop-Process -Force
    $running | Wait-Process -Timeout 5 -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Move-Item -Force $tmp $exe
    Unblock-File $exe -ErrorAction SilentlyContinue
    Say "installed $exe"

    # the agent writes its config with a fresh id on the first run
    $created = -not (Test-Path $conf)
    if ($created) { & $exe --config $conf --dump | Out-Null }
    if (-not (Test-Path $conf)) { Die "the agent did not create $conf" }
    $text = [IO.File]::ReadAllText($conf)
    $current = ''
    if ($text -match '(?m)^token = "(.*)"\s*$') { $current = $Matches[1] }
    if ($env:WINSTATS_NO_TOKEN -eq '1') { $token = '' }
    elseif (-not $token) {
        # a new install gets a token; an update keeps what is configured (even none)
        if (-not $created) { $token = $current }
        else {
            $bytes = New-Object byte[] 16
            [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
            $token = -join ($bytes | ForEach-Object { $_.ToString('x2') })
        }
    }
    function Set-Key([string]$t, [string]$key, [string]$value) {
        $line = "$key = `"" + ($value -replace '\\', '\\' -replace '"', '\"') + '"'
        if ($t -match "(?m)^$key = .*$") { return [regex]::Replace($t, "(?m)^$key = .*$", { param($m) $line }) }
        return $t.TrimEnd() + "`n$line`n"
    }
    $text = Set-Key $text 'token' $token
    if ($name) { $text = Set-Key $text 'name' $name }
    # UTF-8 without a BOM: the TOML reader takes no BOM
    [IO.File]::WriteAllText($conf, $text, (New-Object Text.UTF8Encoding $false))

    & $exe --config $conf install-service | Out-Null
    Start-Process -FilePath $exe -ArgumentList @('--config', "`"$conf`"") -WindowStyle Hidden
    Start-Sleep -Seconds 1
    if (-not (Get-Process winstats-agent -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })) { Die "the agent did not start" }
    Say "agent running; it starts at logon (Run key in the registry)"

    # inbound rules need an administrator; without them Windows asks on the first connection
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($env:WINSTATS_NO_FIREWALL -eq '1') {
        Say "Windows Firewall left as it is"
    } elseif ($admin) {
        Get-NetFirewallRule -DisplayName 'winstats agent*' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
        New-NetFirewallRule -DisplayName 'winstats agent (TCP)' -Direction Inbound -Program $exe -Protocol TCP -LocalPort $Port -Action Allow -Profile Domain, Private | Out-Null
        New-NetFirewallRule -DisplayName 'winstats agent (beacons)' -Direction Inbound -Program $exe -Protocol UDP -LocalPort $BeaconPort -Action Allow -Profile Domain, Private | Out-Null
        Say "Windows Firewall: allowed $Port/tcp and $BeaconPort/udp on private networks"
    } else {
        Warn "not an administrator: Windows Firewall may ask to allow winstats-agent; allow it on private networks"
    }

    # the address of the adapter with the default route first, then the others (VPN, VMs)
    $routed = @(Get-NetIPConfiguration -ErrorAction SilentlyContinue | Where-Object { $_.IPv4DefaultGateway } | ForEach-Object { $_.IPv4Address.IPAddress })
    $other = @(Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object { $_.IPAddress -notmatch '^(127\.|169\.254\.)' -and $routed -notcontains $_.IPAddress } |
        Select-Object -ExpandProperty IPAddress)
    $addrs = @($routed) + @($other) | Select-Object -First 4
    Write-Host ""
    Say "winstats agent is running on port $Port"
    Write-Host ""
    Write-Host "  In winstats (right-click the taskbar strip) > Remote machines:"
    Write-Host "    on the same network the machine shows up by itself: open it and pick Connect;"
    Write-Host "    elsewhere (another subnet, a VPN, a cloud VM): Add by address..."
    Write-Host ""
    foreach ($a in $addrs) { Write-Host "    address  $a" }
    if ($token) { Write-Host "    token    $token" } else { Write-Host "    token    (none: anyone on the network can read this machine)" }
    Write-Host ""
}

Install-WinstatsAgent
