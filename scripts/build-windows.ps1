# build-windows.ps1 — Build the WebRDP server and WASM module
$ErrorActionPreference = "Stop"

Write-Host "=== Building IronRDP WASM module ===" -ForegroundColor Cyan

# Build WASM module (output to web/pkg/)
wasm-pack build wasm/ --release --target web --out-dir ../web/pkg

if ($LASTEXITCODE -ne 0) {
    Write-Host "WASM build failed" -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "=== Building server ===" -ForegroundColor Cyan

cargo build --release -p server

if ($LASTEXITCODE -ne 0) {
    Write-Host "Server build failed" -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "=== Build complete ===" -ForegroundColor Green
Write-Host "Run: .\target\release\server.exe --port 8080 --rdp-target localhost:3389"
