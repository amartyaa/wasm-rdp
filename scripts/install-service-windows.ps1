# install-service-windows.ps1 — Install IronBridge as a Windows Service
param(
    [string]$BinaryPath = ".\server.exe",
    [int]$Port = 8080,
    [string]$RdpTarget = "localhost:3389"
)

$ServiceName = "IronBridgeRDP"
$DisplayName = "IronBridge Web RDP Service"
$Description = "Browser-native RDP client powered by IronRDP"

$AbsPath = (Resolve-Path $BinaryPath).Path
$BinArgs = "$AbsPath --service --port $Port --rdp-target $RdpTarget"

Write-Host "Installing $DisplayName..." -ForegroundColor Cyan

# Remove existing service if present
$existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "Removing existing service..." -ForegroundColor Yellow
    sc.exe stop $ServiceName 2>$null
    sc.exe delete $ServiceName
    Start-Sleep -Seconds 2
}

sc.exe create $ServiceName `
    binPath= "$BinArgs" `
    start= auto `
    DisplayName= "$DisplayName"

sc.exe description $ServiceName "$Description"

Write-Host ""
Write-Host "Service installed successfully!" -ForegroundColor Green
Write-Host "  Name:   $ServiceName"
Write-Host "  Binary: $AbsPath"
Write-Host "  Port:   $Port"
Write-Host "  Target: $RdpTarget"
Write-Host ""
Write-Host "Commands:" -ForegroundColor Cyan
Write-Host "  Start:    sc.exe start $ServiceName"
Write-Host "  Stop:     sc.exe stop $ServiceName"
Write-Host "  Status:   sc.exe query $ServiceName"
Write-Host "  Remove:   sc.exe delete $ServiceName"
