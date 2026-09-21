# MeshMCP Setup & Verification Guide

Comprehensive guide for building, testing, configuring, and running **MeshMCP** locally and integrating it with AI coding agents (Claude Code, Cursor, Windsurf, VS Code).

---

## 1. System Requirements & Prerequisites

MeshMCP is built in zero-copy Rust and uses Tree-sitter parsers compiled via C-FFI.

### Supported Operating Systems
- **macOS**: Apple Silicon (M1/M2/M3/M4) or Intel, macOS 13+ (Ventura, Sonoma, Sequoia)
- **Linux**: x86_64 or aarch64, kernel 5.15+ (glibc 2.31+ or musl)
- **Windows**: Supported via WSL2 (Ubuntu 22.04 / 24.04 recommended)

### Required Tools
- **Rust Toolchain**: 1.80.0 or later (stable). Install via [rustup](https://rustup.rs):
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup update stable
  ```
- **C Compiler**: `clang` or `gcc` (required to compile Tree-sitter C grammar runtimes):
  - macOS: `xcode-select --install`
  - Debian/Ubuntu: `sudo apt update && sudo apt install -y build-essential clang`
  - Fedora/RHEL: `sudo dnf groupinstall "Development Tools"`
- **Git**: 2.30+

---

## 2. Workspace Overview & Crates

MeshMCP is organized as a Cargo workspace with four specialized crates:

| Crate | Path | Role |
| :--- | :--- | :--- |
| **`mesh-core`** | [`crates/mesh-core`](crates/mesh-core) | Core domain logic, `ValidatedScope` security jail, `DifferentialVfs`, in-memory `ContractGraph`, `ArcSwap` state, SQLite WAL audit logger, and background rescan. |
| **`mesh-parsers`** | [`crates/mesh-parsers`](crates/mesh-parsers) | Polyglot Tree-sitter C-FFI runtimes (Java, Go, Python, TypeScript, Rust, Protobuf, YAML), `AstDecapitator` body stripper, `AstGuard` timeout and depth limits, and Markdown generator. |
| **`mesh-server`** | [`crates/mesh-server`](crates/mesh-server) | MCP JSON-RPC stdio protocol framing, CLI subcommands (`doctor`, `init`, `install-hooks`, `run`), and transparent UDS client proxy. |
| **`mesh-daemon`** | [`crates/mesh-daemon`](crates/mesh-daemon) | Background `meshd` server. Multiplexes concurrent agent sessions over a single Unix Domain Socket (`.sock`), runs the single inotify/FSEvents watcher, and shuts down automatically after idle timeout. |

---

## 3. Building the Workspace

### Development Build (Fast Compilation)
```bash
cargo build --workspace
```
Binaries will be placed in:
- `target/debug/mesh-mcp` (Main CLI & MCP stdio proxy)
- `target/debug/meshd` (Background daemon)

### Production Build (Optimized with mimalloc & Thin LTO)
```bash
cargo build --workspace --release
```
Binaries will be placed in:
- `target/release/mesh-mcp` (~6.8 MB)
- `target/release/meshd` (~6.5 MB)

---

## 4. Running Tests & Quality Verification

MeshMCP enforces a zero-warning, zero-compromise engineering standard.

### 4.1 Run the Full Test Suite
```bash
cargo test --workspace
```
This executes **67 automated tests** across all crates:
- **`mesh-core`** (22 tests): Reverse dependency indexing, gRPC flow analysis, differential VFS hashing, symlink escape rejection, Unicode NFC normalization, secret redaction, and multi-threaded SQLite WAL audit logging.
- **`mesh-parsers`** (24 tests): AST body decapitation across Java, Go, TypeScript (including arrow functions), Python, and Rust; Tree-sitter 15ms C-FFI timeouts; AST guard depth limits; and Markdown formatting.
- **`mesh-daemon`** (8 tests): Unix Domain Socket binding, ping/pong protocol, client counter, 20 concurrent multiplexed clients, and idle watchdog timeout auto-shutdown.
- **`mesh-server`** (4 unit + 9 integration tests): Stdio loopback, tool registry, W3C `traceparent` propagation, live hot-reload, and end-to-end MCP tool invocations (`smart_search`, `find_dependents`, `analyze_grpc`, `analyze_impact`, `search_docs`).

### 4.2 Check Code Formatting (CI Requirement)
```bash
cargo fmt --all -- --check
```
To auto-format code according to repo standards:
```bash
cargo fmt --all
```

### 4.3 Run Strict Clippy
```bash
cargo clippy --all-targets -- -D warnings
```
Must exit with 0 warnings.

---

## 5. Diagnostic Healthcheck (`doctor`)

Before connecting MeshMCP to your AI agent, run the built-in diagnostic tool to verify environment readiness, permissions, parsers, and latency:

```bash
cargo run -p mesh-server -- doctor
# or with the release binary:
./target/release/mesh-mcp doctor
```

### Example Diagnostic Output
```
🔍 Running MeshMCP Diagnostic Healthcheck (RFC-001 Rev. 2.9.1)...

✔ Config syntax: Valid (mesh-mcp.toml)
✔ Symlink invariants: follow_links=false verified across all engines
✔ Unicode NFC normalization: Active (APFS/NFC compliant, zero NFD divergence)
✔ Container mount aliases: Configured (Docker / DevContainer bridge ready)
✔ Secret redaction engine: ACTIVE (Dev secrets masked with fallback hints)
✔ Host OS event subsystem: Native (APFS FSEvents/inotify active, 150ms debounced watcher)
✔ Audit log engine: SQLite WAL (audit.db with multi-process concurrent SHA-256 chaining)
✔ Stdio loopback latency: 0.02ms
✔ Tree-sitter parsers initialized (Java, Go, Python, TS [incl. arrow functions], Rust [incl. Tonic macros])
✔ Memory baseline: < 20 MiB RSS (mimalloc + compact_str)

✔ All systems operational. Ready for AI agents.
```

---

## 6. Configuring Your Workspace (`mesh-mcp.toml`)

MeshMCP looks for configuration in:
1. Path passed via `--config <path>`
2. `.agents/mesh-mcp.toml`
3. `mesh-mcp.toml` in the current working directory

### 6.1 Automatic Initialization
To auto-detect repositories, services, and schemas in your current workspace:
```bash
cargo run -p mesh-server -- init --auto
```
This generates a validated `mesh-mcp.toml` tailored to your repository structure.

### 6.2 Manual Configuration Example
Create `mesh-mcp.toml` in your project root:

```toml
[workspace]
name = "my-polyglot-mesh"
version = "2.9.1"
roots = [
  "proto-registry",
  "services/billing-service",
  "services/auth-service",
  "services/api-gateway",
  "docs"
]

# Exclude heavy or sensitive directories from crawler & VFS
exclude_patterns = [
  "**/.git/**",
  "**/node_modules/**",
  "**/target/**",
  "**/dist/**",
  "**/.env*",
  "**/secrets/**",
  "**/*.pem",
  "**/*.key"
]

# Bridge Docker/container paths to host paths
[workspace.mount_aliases]
"/app" = "."

# Active Governance rules: prevent direct modification of critical contract repositories
[engines.policy.stop_rules]
"proto-registry" = "STOP: Contract schemas must be reviewed and published independently before service updates."

[engines.watcher]
enabled = true
debounce_ms = 150

[engines.audit]
db_path = "~/.cache/mesh-mcp/audit.db"
```

---

## 7. Running MeshMCP

MeshMCP supports two operating modes:

### Mode A: Daemon Architecture (Default & Recommended)
In this mode, `mesh-mcp run` acts as an ultra-lightweight client proxy (< 2 MiB memory):
1. It looks for a running `meshd` background daemon on the local Unix Domain Socket.
2. If `meshd` is not running, it automatically spawns it in the background.
3. It transparently bridges stdio JSON-RPC requests to the daemon over UDS.
4. The daemon keeps the in-memory architecture graph hot, handles file changes with a single OS watcher, and auto-terminates after idle timeout when all clients disconnect.

```bash
# Run via cargo (auto-spawns daemon)
cargo run -p mesh-server -- run

# Or using the built binary
./target/release/mesh-mcp run
```

#### Socket Path Resolution
The Unix Domain Socket path is determined in the following priority:
1. Environment variable `$MESH_SOCKET_PATH` (if set)
2. `$XDG_RUNTIME_DIR/mesh/meshd.sock` (Linux)
3. `$HOME/.cache/mesh/meshd.sock` (macOS / Linux)
4. `/tmp/mesh-<UID>.sock` (Fallback)

### Mode B: Standalone Mode
If you prefer running a self-contained, single-process instance without a background daemon (useful in air-gapped CI or containers):

```bash
cargo run -p mesh-server -- run --standalone --config mesh-mcp.toml
# or
./target/release/mesh-mcp run --standalone --config mesh-mcp.toml
```

---

## 8. Manual Testing via JSON-RPC Stdio

You can verify the MCP server directly using `echo` or standard input pipes:

### 8.1 List Available Tools (`tools/list`)
```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' | ./target/release/mesh-mcp run --standalone
```
Response contains definitions and JSON schemas for all 5 tools: `smart_search`, `find_dependents`, `analyze_grpc`, `analyze_impact`, `search_docs`.

### 8.2 Perform AST-Decapitated Code Search (`smart_search`)
```bash
echo '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"smart_search","arguments":{"query":"fn ","scope":"crates/mesh-core","include_body":false}}}' | ./target/release/mesh-mcp run --standalone
```
Notice that functions are returned with their signatures and docstrings, with implementation bodies replaced by `{ /* stripped */ }`.

### 8.3 Query Reverse Dependencies (`find_dependents`)
```bash
echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"find_dependents","arguments":{"target":"ContractGraph"}}}' | ./target/release/mesh-mcp run --standalone
```

---

## 9. AI IDE & MCP Client Integration

Add MeshMCP to your agent configuration.

### 9.1 Cursor & Windsurf
Add to `.cursor/mcp.json` (or `~/.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "mesh-mcp": {
      "command": "/absolute/path/to/causalmesh/target/release/mesh-mcp",
      "args": ["run", "--config", "/absolute/path/to/mesh-mcp.toml"]
    }
  }
}
```

### 9.2 Claude Code
Add to your Claude Code MCP settings (`~/.claude/claude_code_config.json` or run `claude mcp add`):

```bash
claude mcp add mesh-mcp -- /absolute/path/to/causalmesh/target/release/mesh-mcp run --config /absolute/path/to/mesh-mcp.toml
```

Or manually in `~/.claude.json`:
```json
{
  "mcpServers": {
    "mesh-mcp": {
      "command": "/absolute/path/to/causalmesh/target/release/mesh-mcp",
      "args": ["run", "--config", "/absolute/path/to/mesh-mcp.toml"]
    }
  }
}
```

### 9.3 Claude Desktop
Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "mesh-mcp": {
      "command": "/absolute/path/to/causalmesh/target/release/mesh-mcp",
      "args": ["run", "--config", "/absolute/path/to/mesh-mcp.toml"]
    }
  }
}
```

---

## 10. Installing Active Governance Git Hooks

To install physical pre-commit hooks that prevent autonomous agents from committing direct changes to guarded repositories:

```bash
./target/release/mesh-mcp install-hooks
```
This writes an executable script into `.git/hooks/pre-commit` that inspects staged files and enforces the rules defined in `[engines.policy.stop_rules]`.

---

## 11. Troubleshooting & FAQ

### Stale Socket File
If the daemon was forcefully killed (`kill -9`), a stale `.sock` file might remain on disk.
- **Resolution**: `mesh-mcp` automatically detects stale sockets on startup. You can also manually remove the socket:
  ```bash
  rm -f ~/.cache/mesh/meshd.sock /tmp/mesh-*.sock
  ```

### Linux inotify Watcher Limit
On large workspaces (10,000+ files) on Linux, inotify watcher limits may be reached.
- **Check limit**: `cat /proc/sys/fs/inotify/max_user_watches`
- **Resolution**: Increase limit to 524,288:
  ```bash
  sudo sysctl -w fs.inotify.max_user_watches=524288
  echo "fs.inotify.max_user_watches=524288" | sudo tee -a /etc/sysctl.d/99-inotify.conf
  ```

### C-FFI Tree-sitter Compiler Errors
If building fails on Tree-sitter C files:
- Verify that `clang` or `gcc` is in your `$PATH`.
- On macOS, ensure Xcode command line tools are installed: `xcode-select --install`.

### Checking Audit Logs
All tool calls and operations are logged in SQLite Write-Ahead Logging format:
```bash
sqlite3 ~/.cache/mesh-mcp/audit.db "SELECT timestamp, session_id, tool_name, hash FROM audit_log ORDER BY id DESC LIMIT 10;"
```
