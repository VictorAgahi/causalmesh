#!/usr/bin/env bash
# scripts/ci.sh — Local Continuous Integration & Invariants Verification for MeshMCP
set -euo pipefail

echo "======================================================================"
echo "🚀 Running MeshMCP Local CI & Quality Pipeline"
echo "======================================================================"

# Step 1: Rustfmt check
echo "👉 [1/6] Checking code formatting (cargo fmt)..."
cargo fmt --all -- --check
echo "✔ Rustfmt check passed."

# Step 2: Strict Clippy check
echo "👉 [2/6] Running strict Clippy lints (-D warnings)..."
cargo clippy --workspace --all-targets -- -D warnings
echo "✔ Clippy check passed with 0 warnings."

# Step 3: Run all workspace tests
echo "👉 [3/6] Running complete test suite (33 unit & integration tests)..."
cargo test --workspace --verbose
echo "✔ All tests passed."

# Step 4: Build release binary with Thin LTO
echo "👉 [4/6] Compiling production release binary (Thin LTO & mimalloc)..."
cargo build --workspace --release
echo "✔ Release binary compiled successfully."

# Step 5: Smoke test binary CLI & doctor
echo "👉 [5/6] Executing smoke tests on release binary..."
./target/release/mesh-mcp --version
./target/release/mesh-mcp doctor
echo "✔ Diagnostic healthcheck passed."

# Step 6: Test JSON-RPC stdio loopback
echo "👉 [6/6] Testing JSON-RPC 2.0 stdio protocol handshake loopback..."
PAYLOAD='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"local-ci","version":"1.0.0"}}}'
RESPONSE=$(echo "$PAYLOAD" | ./target/release/mesh-mcp run)
echo "Server response: $RESPONSE"
echo "$RESPONSE" | grep -q '"jsonrpc":"2.0"'
echo "$RESPONSE" | grep -q '"name":"mesh-mcp"'
echo "✔ Stdio JSON-RPC handshake verified."

echo "======================================================================"
echo "🎉 ALL CI CHECKS PASSED: MeshMCP is certified production-ready!"
echo "======================================================================"
