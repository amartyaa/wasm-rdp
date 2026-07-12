# install-service-windows.ps1 — Install IronBridge as a Windows Service
param(
    [string]$BinaryPath = "C:\Users\amart\Downloads\web-rdp-rust\target\release\server.exe",
    [int]$Port = 6081,
    [string]$RdpTarget = "localhost:3389",
    # Optional RemoteApp (RAIL) catalog: "Name=path" entries, ';'-separated.
    # Empty (default) = plain full-desktop proxy. Paths with spaces are fine —
    # they're double-quoted in the ImagePath below. Example:
    #   -RemoteApps "Microsoft Edge=C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
    [string]$RemoteApps = "Microsoft Edge=C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe;Chrome=C:\Program Files\Google\Chrome\Application\chrome.exe"
)

$ServiceName = "IronBridgeRDP"
$DisplayName = "IronBridge Web RDP Service"
$Description = "Browser-native RDP client powered by IronRDP"

$AbsPath = (Resolve-Path $BinaryPath).Path

# Build the service ImagePath. Windows launches the service by parsing this
# string with CommandLineToArgvW, which honors ONLY double quotes — single
# quotes are literal characters. So any path with spaces (the exe, or a RemoteApp
# path) MUST be wrapped in double quotes, or the SCM splits it into stray args,
# clap rejects them, and the process exits before signaling RUNNING — surfacing
# as SCM error 1053 ("did not respond in a timely fashion"). New-Service passes
# this string straight to the CreateService API, so the embedded `"` land
# verbatim in the registry (no native-exe command-line re-escaping to fight,
# unlike `sc.exe create binPath=`).
$BinArgs = "`"$AbsPath`" --service --port $Port --rdp-target $RdpTarget"
if ($RemoteApps) {
    $BinArgs += " --enable-remote-app --app `"$RemoteApps`""
}

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
    Write-Host "  Apps:   $RemoteApps"
}
Write-Host ""
Write-Host "Commands:" -ForegroundColor Cyan
Write-Host "  Start:    sc.exe start $ServiceName"
Write-Host "  Stop:     sc.exe stop $ServiceName"
Write-Host "  Status:   sc.exe query $ServiceName"
Write-Host "  Remove:   sc.exe delete $ServiceName"
