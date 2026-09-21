# install-service-windows.ps1 — Install IronBridge as a Windows Service
param(
    [string]$BinaryPath = "C:\Users\amart\Downloads\web-rdp-rust\target\release\server.exe",
    [int]$Port = 6081,
    [string]$RdpTarget = "localhost:3389",
    # Optional RemoteApp (RAIL) catalog to seed. ';'-separated "Name=path" entries.
    # Written to remote-apps.txt beside the binary (NOT baked into the service
    # ImagePath), so you can publish/unpublish later by editing that file — no
    # reinstall, no restart. Empty = plain full-desktop proxy. Example:
    #   -RemoteApps "Microsoft Edge=C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
    [string]$RemoteApps = "Microsoft Edge=C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe;Chrome=C:\Program Files\Google\Chrome\Application\chrome.exe"
)

$ServiceName = "IronBridgeRDP"
$DisplayName = "IronBridge Web RDP Service"
$Description = "Browser-native RDP client powered by IronRDP"

$AbsPath = (Resolve-Path $BinaryPath).Path

# Phase-1 dynamic publishing: seed the live catalog file next to the binary, one
# "Name=path" per line (UTF-8, no BOM). The server reads it on every login-page
# render, so the catalog is editable without touching the service. Line-delimited
# means paths with spaces need NO quoting — this sidesteps the ImagePath /
# CommandLineToArgvW quoting trap entirely (no --app in the ImagePath at all).
if ($RemoteApps) {
    $AppsFile = Join-Path (Split-Path $AbsPath) "remote-apps.txt"
    $lines = $RemoteApps.Split(';') | Where-Object { $_ -match '=' } | ForEach-Object { $_.Trim() }
    [System.IO.File]::WriteAllLines($AppsFile, $lines)
    Write-Host "Seeded RemoteApp catalog: $AppsFile ($($lines.Count) app(s))" -ForegroundColor DarkGray
}

# Service ImagePath: no RemoteApp flags — the catalog file above drives publishing.
# The exe path is still double-quoted so an install path with spaces is safe.
$BinArgs = "`"$AbsPath`" --service --port $Port --rdp-target $RdpTarget"

Write-Host "Installing $DisplayName..." -ForegroundColor Cyan

# Remove existing service if present
$existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "Removing existing service..." -ForegroundColor Yellow
    sc.exe stop $ServiceName 2>$null
    sc.exe delete $ServiceName
    Start-Sleep -Seconds 2
}

New-Service -Name $ServiceName -BinaryPathName $BinArgs -DisplayName $DisplayName -StartupType Automatic | Out-Null
sc.exe description $ServiceName "$Description"
sc.exe start $ServiceName
Write-Host ""
Write-Host "Service installed successfully!" -ForegroundColor Green
Write-Host "  Name:   $ServiceName"
Write-Host "  Binary: $AbsPath"
Write-Host "  Port:   $Port"
Write-Host "  Target: $RdpTarget"
if ($RemoteApps) {
    Write-Host "  Apps:   edit $AppsFile to publish/unpublish (no restart)"
}
Write-Host ""
Write-Host "Commands:" -ForegroundColor Cyan
Write-Host "  Start:    sc.exe start $ServiceName"
Write-Host "  Stop:     sc.exe stop $ServiceName"
Write-Host "  Status:   sc.exe query $ServiceName"
Write-Host "  Remove:   sc.exe delete $ServiceName"
