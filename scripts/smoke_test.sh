#!/usr/bin/env bash
set -euo pipefail

echo "==> Building mesh-mcp in release mode..."
cargo build --release --bin mesh-mcp

BINARY="./target/release/mesh-mcp"

echo "==> Verifying --version..."
$BINARY --version

echo "==> Running doctor on examples/polyglot-shop..."
(cd examples/polyglot-shop && "../../$BINARY" doctor)

echo "==> Running doctor on examples/volontariapp-fixture..."
(cd examples/volontariapp-fixture && "../../$BINARY" doctor)

echo "==> Running graph on examples/polyglot-shop..."
(cd examples/polyglot-shop && "../../$BINARY" graph)

echo "✔ All smoke tests passed successfully!"
