#!/bin/bash
# build-ubuntu.sh — Build the WebRDP server and WASM module
set -euo pipefail

echo "=== Building IronRDP WASM module ==="

# Build WASM module (output to web/pkg/)
wasm-pack build wasm/ --release --target web --out-dir ../web/pkg

echo ""
echo "=== Building server ==="

cargo build --release -p server

echo ""
echo "=== Build complete ==="
echo "Run: ./target/release/server --port 8080 --rdp-target localhost:3389"
