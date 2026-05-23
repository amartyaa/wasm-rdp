#!/bin/bash
set -e
source "$HOME/.cargo/env"

echo "=== Building IronRDP WASM module ==="
cd /mnt/c/Users/amart/Downloads/web-rdp-rust
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' wasm-pack build wasm/ --release --target web --out-dir ../web/pkg

echo ""
echo "=== Building server ==="
cargo build --release -p server

echo ""
echo "=== Build complete ==="
echo "Run: ./target/release/server --port 8080 --rdp-target localhost:3389"
