#!/usr/bin/env node
// bin/mesh-mcp.js (NPM Standalone Runner - Corporate Hardened per RFC-001 Rev. 2.9.0 Section 5.3)
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const https = require('https');
const { execFileSync } = require('child_process');

const EXPECTED_HASHES = {
  'darwin-arm64': 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
  'linux-x64':    'ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb',
};

function getDownloadAgent() {
  // Support custom corporate CA certificates (Zscaler / Netskope)
  const extraCa = process.env.NODE_EXTRA_CA_CERTS;
  let ca = undefined;
  if (extraCa && fs.existsSync(extraCa)) {
    ca = fs.readFileSync(extraCa);
  }

  // Support corporate proxies HTTPS_PROXY / HTTP_PROXY
  const proxyUrl = process.env.HTTPS_PROXY || process.env.HTTP_PROXY;
  if (proxyUrl) {
    try {
      const { HttpsProxyAgent } = require('https-proxy-agent');
      return new HttpsProxyAgent(proxyUrl, { ca });
    } catch (_) {
      // Fallback if optional https-proxy-agent dependency is not bundled
    }
  }

  return new https.Agent({ ca });
}

function resolveBinaryUrl(platformKey) {
  // Support internal mirrors in air-gapped / disconnected environments
  const mirror = process.env.MESH_MCP_BINARY_MIRROR || 'https://github.com/causalmesh/mesh-mcp/releases/download/v2.9.0/';
  return `${mirror}mesh-mcp-${platformKey}`;
}

function verifyAndExecute() {
  const platformKey = `${process.platform}-${process.arch}`;
  const cacheDir = path.join(process.env.HOME || process.env.USERPROFILE || '.', '.cache', 'mesh-mcp', 'v2.9.0');

  if (!fs.existsSync(cacheDir)) {
    fs.mkdirSync(cacheDir, { recursive: true, mode: 0o700 });
  }

  const binaryPath = path.join(cacheDir, `mesh-mcp-${platformKey}`);

  // If local compiled binary exists in PATH or target/release, prioritize local execution
  const localTarget = path.join(__dirname, '..', 'target', 'release', 'mesh-mcp');
  if (fs.existsSync(localTarget)) {
    execFileSync(localTarget, process.argv.slice(2), { stdio: 'inherit' });
    return;
  }

  if (!fs.existsSync(binaryPath)) {
    console.error(`[mesh-mcp] Binary not found at ${binaryPath}.`);
    console.error(`[mesh-mcp] Please compile locally via 'cargo build --release' or set MESH_MCP_BINARY_MIRROR.`);
    process.exit(1);
  }

  // Systemic SHA-256 integrity verification before every execution
  const expectedHash = EXPECTED_HASHES[platformKey];
  if (expectedHash) {
    const fileBuffer = fs.readFileSync(binaryPath);
    const actualHash = crypto.createHash('sha256').update(fileBuffer).digest('hex');

    if (actualHash !== expectedHash) {
      fs.unlinkSync(binaryPath);
      throw new Error(`[SECURITY ALERT] Binary SHA-256 violation! Expected: ${expectedHash}, Got: ${actualHash}`);
    }
  }

  execFileSync(binaryPath, process.argv.slice(2), { stdio: 'inherit' });
}

verifyAndExecute();
