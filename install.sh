#!/usr/bin/env bash
# CausalMesh (MeshMCP) — Official 1-Liner Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/VictorAgahi/causalmesh/main/install.sh | bash

set -euo pipefail

REPO="VictorAgahi/causalmesh"
VERSION="${MESH_VERSION:-latest}"

BOLD="\033[1m"
GREEN="\033[32m"
CYAN="\033[36m"
YELLOW="\033[33m"
RED="\033[31m"
RESET="\033[0m"

echo -e "${CYAN}${BOLD}"
echo "  ____                         _ __  __           _     "
echo " / ___|__ _ _   _ ___  __ _  | |  \/  | ___  ___ | |__  "
echo "| |   / _\` | | | / __|/ _\` | | | |\/| |/ _ \/ __|| '_ \ "
echo "| |__| (_| | |_| \__ \ (_| | | | |  | |  __/\__ \| | | |"
echo " \____\__,_|\__,_|___/\__,_| |_|_|  |_|\___||___/|_| |_|"
echo -e "${RESET}"
echo -e "${BOLD}Universal Polyglot Architecture Mesh & Contract Governance MCP Server${RESET}\n"

# 1. Detect OS and Architecture
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$ARCH" in
  x86_64|amd64)
    TARGET_ARCH="x86_64"
    ;;
  aarch64|arm64)
    TARGET_ARCH="aarch64"
    ;;
  *)
    echo -e "${RED}✖ Unsupported architecture: $ARCH${RESET}"
    exit 1
    ;;
esac

case "$OS" in
  linux)
    TARGET_OS="unknown-linux-gnu"
    EXT="tar.gz"
    ;;
  darwin)
    TARGET_OS="apple-darwin"
    EXT="tar.gz"
    ;;
  msys*|mingw*|cygwin*)
    TARGET_OS="pc-windows-msvc"
    EXT="zip"
    ;;
  *)
    echo -e "${RED}✖ Unsupported operating system: $OS${RESET}"
    exit 1
    ;;
esac

TARGET_TRIPLE="${TARGET_ARCH}-${TARGET_OS}"
echo -e "✔ Detected target: ${GREEN}${TARGET_TRIPLE}${RESET}"

# 2. Determine download URL
if [ "$VERSION" = "latest" ]; then
  DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/mesh-mcp-${TARGET_TRIPLE}.${EXT}"
else
  DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/mesh-mcp-${TARGET_TRIPLE}.${EXT}"
fi

# 3. Determine install destination
INSTALL_DIR="${MESH_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$INSTALL_DIR"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo -e "⬇ Downloading CausalMesh from ${CYAN}${DOWNLOAD_URL}${RESET}..."

DOWNLOAD_SUCCESS=0
if command -v curl >/dev/null 2>&1; then
  if curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/archive.${EXT}" 2>/dev/null; then
    DOWNLOAD_SUCCESS=1
  fi
elif command -v wget >/dev/null 2>&1; then
  if wget -qO "$TMP_DIR/archive.${EXT}" "$DOWNLOAD_URL" 2>/dev/null; then
    DOWNLOAD_SUCCESS=1
  fi
else
  echo -e "${RED}✖ Neither curl nor wget found. Please install one to proceed.${RESET}"
  exit 1
fi

if [ "$DOWNLOAD_SUCCESS" -eq 1 ]; then
  echo -e "📦 Unpacking binaries into ${INSTALL_DIR}..."
  if [ "$EXT" = "tar.gz" ]; then
    tar -xzf "$TMP_DIR/archive.${EXT}" -C "$TMP_DIR"
  elif [ "$EXT" = "zip" ]; then
    unzip -q "$TMP_DIR/archive.${EXT}" -d "$TMP_DIR"
  fi

  # Move binaries
  if [ -f "$TMP_DIR/mesh-mcp" ]; then
    mv "$TMP_DIR/mesh-mcp" "$INSTALL_DIR/mesh-mcp"
    chmod +x "$INSTALL_DIR/mesh-mcp"
  elif [ -f "$TMP_DIR/mesh-mcp.exe" ]; then
    mv "$TMP_DIR/mesh-mcp.exe" "$INSTALL_DIR/mesh-mcp.exe"
  fi

  if [ -f "$TMP_DIR/meshd" ]; then
    mv "$TMP_DIR/meshd" "$INSTALL_DIR/meshd"
    chmod +x "$INSTALL_DIR/meshd"
  elif [ -f "$TMP_DIR/meshd.exe" ]; then
    mv "$TMP_DIR/meshd.exe" "$INSTALL_DIR/meshd.exe"
  fi
else
  echo -e "${YELLOW}ℹ Precompiled binary release not yet published on GitHub Releases.${RESET}"
  if command -v cargo >/dev/null 2>&1; then
    echo -e "⚙ Building and installing locally via Cargo..."
    cargo install --git "https://github.com/${REPO}.git" mesh-server --bin mesh-mcp --root "$HOME/.local"
    cargo install --git "https://github.com/${REPO}.git" mesh-daemon --bin meshd --root "$HOME/.local"
  else
    echo -e "${RED}✖ Failed to download release archive from GitHub and Cargo is not installed.${RESET}"
    echo -e "  Please ensure the repository has published release binaries at:"
    echo -e "  https://github.com/${REPO}/releases"
    exit 1
  fi
fi

echo -e "\n${GREEN}${BOLD}✔ CausalMesh installed successfully!${RESET}"

# Check PATH
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
  echo -e "\n${YELLOW}⚠ Action Required: Add $INSTALL_DIR to your PATH:${RESET}"
  echo -e "  export PATH=\"\$HOME/.local/bin:\$PATH\""
  echo -e "  (Add the line above to your ~/.bashrc or ~/.zshrc)\n"
fi

echo -e "${CYAN}Next Steps:${RESET}"
echo -e "  1. Scan and initialize your monorepo:"
echo -e "     ${BOLD}mesh-mcp init --auto${RESET}"
echo -e "  2. View your architecture topology in your browser:"
echo -e "     ${BOLD}mesh-mcp graph --open${RESET}"
echo -e "  3. Verify system health and IDE hooks:"
echo -e "     ${BOLD}mesh-mcp doctor${RESET}"
echo ""
