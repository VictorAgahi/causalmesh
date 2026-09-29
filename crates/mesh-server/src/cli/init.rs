use std::fs;
use std::path::Path;

pub struct InitCommand;

impl InitCommand {
    /// Writes `.agents/mesh-mcp.toml` from the directory layout — unless one
    /// already exists and `force` is off: a hand-tuned config (roots, event
    /// patterns, stop rules) is the user's, and 7.0.0 silently replaced it on
    /// every `init`, including `init --write-ide-config`. With
    /// `write_ide_config`, also merges a `mesh-mcp` server entry into Claude
    /// Code's `.mcp.json`, `.cursor/mcp.json` and `.vscode/mcp.json`, and the
    /// routing guidance into `CLAUDE.md` and `AGENTS.md` (see
    /// [`Self::write_agent_guidance`]).
    pub fn run(
        _auto: bool,
        write_ide_config: bool,
        force: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cur_dir = std::env::current_dir()?;
        let config_file = cur_dir.join(".agents").join("mesh-mcp.toml");
        if config_file.exists() && !force {
            eprintln!(
                "ℹ Kept the existing .agents/mesh-mcp.toml (pass --force to regenerate it from the directory layout)."
            );
        } else {
            Self::write_generated_config(&cur_dir)?;
        }

        if write_ide_config {
            eprintln!("\n🔌 Auto-Configuring IDEs (--write-ide-config):");
            Self::configure_claude_code(&cur_dir)?;
            Self::configure_cursor(&cur_dir)?;
            Self::configure_vscode(&cur_dir)?;
            Self::write_agent_guidance(&cur_dir)?;
        }

        eprintln!(
            "\nNext: run `mesh-mcp doctor`, then restart your agent so it loads the mesh-mcp tools."
        );
        Ok(())
    }

    fn write_generated_config(cur_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("🔍 Scanning workspace tree for polyglot services and schemas...");

        let mut roots = Vec::new();

        if cur_dir.join("proto-registry").exists()
            || cur_dir.join("proto").exists()
            || cur_dir.join("protos").exists()
        {
            eprintln!("  Found: Protobuf schemas");
            roots.push("./proto*".to_string());
        }

        // Whichever of these actually exists is the root pushed — a repo with
        // a plain `gateway/` directory (no `api-gateway/`) used to still push
        // the literal, nonexistent path `./api-gateway`, which would never
        // match anything once the generated config was loaded.
        if let Some(gateway_dir) = ["api-gateway", "gateway"]
            .into_iter()
            .find(|d| cur_dir.join(d).exists())
        {
            eprintln!("  Found: API Gateway");
            roots.push(format!("./{gateway_dir}"));
        }

        if cur_dir.join("services").exists() {
            eprintln!("  Found: Microservices directory (./services/*)");
            roots.push("./services/*".to_string());
        }

        if cur_dir.join("crates").exists() {
            eprintln!("  Found: Rust multi-crate workspace (./crates/*)");
            roots.push("./crates/*".to_string());
        } else if cur_dir.join("Cargo.toml").exists() {
            eprintln!("  Found: Rust project (Cargo.toml)");
            // No crates/ subdir to scope to — a single-crate project's source
            // lives directly under the repo root, so root the whole repo.
            roots.push(".".to_string());
        }

        let is_monorepo = cur_dir.join("pnpm-workspace.yaml").exists()
            || cur_dir.join("lerna.json").exists()
            || cur_dir.join("nx.json").exists()
            || cur_dir.join("turbo.json").exists()
            || cur_dir.join("go.work").exists();
        if is_monorepo {
            eprintln!("  Found: Monorepo workspace configuration (pnpm/nx/turbo/go.work)");
            roots.push(".".to_string());
        }

        if cur_dir.join("packages").exists() {
            eprintln!("  Found: Monorepo packages (./packages/*)");
            roots.push("./packages/*".to_string());
        } else if cur_dir.join("package.json").exists() {
            eprintln!("  Found: Node / TypeScript project (package.json)");
            // No packages/ subdir — same reasoning as the Cargo.toml-only case above.
            roots.push(".".to_string());
        }

        // Container directories (`src/`, `apps/`) holding multiple per-service
        // projects, each with its own language marker one level down.
        // Require >= 2 matching subdirectories so an ordinary single-project
        // directory does not produce an extra root.
        for container in ["src", "apps"] {
            let container_dir = cur_dir.join(container);
            if !container_dir.is_dir() {
                continue;
            }
            let marker_count = fs::read_dir(&container_dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|e| e.path().is_dir() && Self::has_language_marker(&e.path()))
                        .count()
                })
                .unwrap_or(0);
            if marker_count >= 2 {
                eprintln!(
                    "  Found: Polyglot service container (./{container}/*, {marker_count} services)"
                );
                roots.push(format!("./{container}/*"));
            }
        }

        if cur_dir.join("go.mod").exists() {
            eprintln!("  Found: Go module (go.mod)");
            // Go has no single canonical source subdirectory (cmd/, pkg/,
            // internal/, or a flat layout all vary), so root the whole repo.
            roots.push(".".to_string());
        }

        if cur_dir.join("pom.xml").exists() || cur_dir.join("build.gradle").exists() {
            eprintln!("  Found: Java / Gradle / Maven project");
            // Maven/Gradle layouts vary too much (single module vs. multi-module
            // src/ trees) to target a subdirectory reliably — root the whole repo.
            roots.push(".".to_string());
        }

        if cur_dir.join("pyproject.toml").exists() || cur_dir.join("requirements.txt").exists() {
            eprintln!("  Found: Python project");
            // Python layout (src/, flat package, ...) isn't standardized either.
            roots.push(".".to_string());
        }

        if cur_dir.join("k8s-infrastructure").exists() || cur_dir.join("deploy").exists() {
            eprintln!("  Found: Kubernetes manifests");
            roots.push("./k8s*".to_string());
            roots.push("./deploy*".to_string());
        }

        // Same "push whichever literal string won the ||, regardless of which
        // directory actually exists" bug as the gateway detection above: a
        // repo with only `architecture/` (no `docs/`) used to get a `./docs`
        // root that could never match anything.
        if let Some(docs_dir) = ["docs", "architecture"]
            .into_iter()
            .find(|d| cur_dir.join(d).exists())
        {
            eprintln!("  Found: Architecture docs");
            roots.push(format!("./{docs_dir}"));
        }

        if roots.is_empty() {
            roots.push(".".to_string());
        }

        // Several of the blocks above can independently push "." for the same
        // repo (e.g. a repo with both a bare Cargo.toml and a pyproject.toml) —
        // dedupe before it hits the emitted TOML array.
        roots.sort();
        roots.dedup();

        // Generate .agents/mesh-mcp.toml
        let config_dir = cur_dir.join(".agents");
        fs::create_dir_all(&config_dir)?;
        let config_file = config_dir.join("mesh-mcp.toml");

        // Every root above is relative to `cur_dir` (where `init --auto` was run),
        // but the config file itself is written one level below it, in `.agents/`.
        // All consumers (doctor/run/graph) resolve `roots` relative to the config
        // file's own parent directory, so a bare "./x" here would silently resolve
        // to ".agents/x" and never match — climb back up to `cur_dir` first.
        let roots: Vec<String> = roots
            .iter()
            .map(|r| {
                if r == "." {
                    "..".to_string()
                } else {
                    r.replacen("./", "../", 1)
                }
            })
            .collect();

        let roots_toml = roots
            .iter()
            .map(|r| format!("\"{}\"", r))
            .collect::<Vec<_>>()
            .join(", ");

        // Determine docs paths dynamically
        let mut doc_paths = Vec::new();
        if cur_dir.join("docs").exists() {
            doc_paths.push("\"${workspace_root}/docs\"".to_string());
        }
        if cur_dir.join("architecture").exists() {
            doc_paths.push("\"${workspace_root}/architecture\"".to_string());
        }
        // If no explicit docs folder exists, check for markdown files in root
        if doc_paths.is_empty() {
            if let Ok(entries) = fs::read_dir(cur_dir) {
                let has_md = entries
                    .flatten()
                    .any(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("md"));
                if has_md {
                    doc_paths.push("\"${workspace_root}/*.md\"".to_string());
                }
            }
        }
        let doc_paths_toml = if doc_paths.is_empty() {
            "[\"${workspace_root}/*.md\"]".to_string()
        } else {
            format!("[{}]", doc_paths.join(", "))
        };

        // Determine stop_rules dynamically based on actual directory layout
        let mut stop_rules = Vec::new();
        let proto_dir = if cur_dir.join("proto-registry").exists() {
            Some("proto-registry")
        } else if cur_dir.join("proto").exists() {
            Some("proto")
        } else if cur_dir.join("protos").exists() {
            Some("protos")
        } else {
            None
        };
        if let Some(p) = proto_dir {
            stop_rules.push(format!("\"{p}\" = \"🛑 STOP CASCADE CI : Contract definitions in '{p}' generate polyglot stubs. Do not modify dependent services directly!\""));
        }

        let infra_dir = if cur_dir.join("k8s-infrastructure").exists() {
            Some("k8s-infrastructure")
        } else if cur_dir.join("k8s").exists() {
            Some("k8s")
        } else if cur_dir.join("deploy").exists() {
            Some("deploy")
        } else if cur_dir.join("deployments").exists() {
            Some("deployments")
        } else {
            None
        };
        if let Some(infra) = infra_dir {
            stop_rules.push(format!("\"{infra}\" = \"🛑 STOP INFRASTRUCTURE : Deployment manifests in '{infra}' require infrastructure review!\""));
        }

        let stop_rules_section = if stop_rules.is_empty() {
            "".to_string()
        } else {
            format!("\n[engines.policy.stop_rules]\n{}\n", stop_rules.join("\n"))
        };

        let workspace_name = cur_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("mesh-workspace");
        let pkg_version = env!("CARGO_PKG_VERSION");

        let toml_content = format!(
            r#"# mesh-mcp.toml — Generated by 'mesh-mcp init --auto'
[workspace]
name = "{workspace_name}"
version = "{pkg_version}"
workspace_root = "${{WORKSPACE_ROOT:-..}}"
roots = [{roots_toml}]

[engines.docs]
enabled = true
paths = {doc_paths_toml}
exact_phrase_boost = 60
fuzzy_fallback = true

[engines.contracts]
enabled = true

[engines.policy]
enabled = true
enforce_git_hooks = true
cryptographic_audit_trail = true
{stop_rules_section}"#
        );

        fs::write(&config_file, toml_content)?;
        eprintln!("\n✨ Generated .agents/mesh-mcp.toml with dynamic root expansion.");
        Ok(())
    }

    /// Upserts the MeshMCP routing section into the workspace's `CLAUDE.md`
    /// (Claude Code) and `AGENTS.md` (Codex, Cursor, Antigravity and other
    /// agents that read it), between [`GUIDANCE_BEGIN`] and [`GUIDANCE_END`]:
    /// the rest of each file is the user's and is left as is.
    ///
    /// Why a project file and not only `initialize.instructions` (7.0.19):
    /// agents weigh MCP server instructions as third-party text. In the
    /// Volontariapp bench, Sonnet ignored the server's routing on 3 of 3
    /// runs and twice said why ("untrusted source", "opaque tool's claim");
    /// the same routing in a project `CLAUDE.md` was followed on 3 of 3.
    fn write_agent_guidance(cur_dir: &Path) -> Result<(), std::io::Error> {
        let team_skill = cur_dir
            .join(".agents")
            .join("skills")
            .join("mesh-mcp")
            .join("SKILL.md")
            .is_file();
        for (file, for_claude_code) in [("CLAUDE.md", true), ("AGENTS.md", false)] {
            let block = agent_guidance(for_claude_code, team_skill);
            if upsert_guidance(&cur_dir.join(file), &block)? {
                eprintln!("  ✔ Agent guidance: MeshMCP routing section written to {file}");
            }
        }
        Ok(())
    }

    /// Claude Code's project-scope config: `.mcp.json` at the workspace root,
    /// servers under `mcpServers` (same file `claude mcp add -s project` writes).
    fn configure_claude_code(cur_dir: &Path) -> Result<(), std::io::Error> {
        let entry = serde_json::json!({ "type": "stdio", "command": "mesh-mcp", "args": [] });
        Self::upsert_mcp_server(&cur_dir.join(".mcp.json"), "mcpServers", entry, None).map(
            |written| {
                if written {
                    eprintln!("  ✔ Claude Code: Added 'mesh-mcp' entry to .mcp.json");
                }
            },
        )
    }

    /// True if `dir` looks like the root of a single project in some
    /// supported language (i.e. carries one of the standard package/module
    /// manifest files this repo already knows how to detect at the top
    /// level).
    fn has_language_marker(dir: &Path) -> bool {
        const MARKERS: &[&str] = &[
            "go.mod",
            "package.json",
            "pyproject.toml",
            "requirements.txt",
            "pom.xml",
            "build.gradle",
            "Cargo.toml",
        ];
        MARKERS.iter().any(|m| dir.join(m).exists())
    }

    fn configure_cursor(cur_dir: &Path) -> Result<(), std::io::Error> {
        let entry = serde_json::json!({ "command": "mesh-mcp", "args": [] });
        Self::upsert_mcp_server(
            &cur_dir.join(".cursor").join("mcp.json"),
            "mcpServers",
            entry,
            None,
        )
        .map(|written| {
            if written {
                eprintln!("  ✔ Detected Cursor: Added 'mesh-mcp' entry to .cursor/mcp.json");
            }
        })
    }

    fn configure_vscode(cur_dir: &Path) -> Result<(), std::io::Error> {
        // VS Code's workspace `mcp.json` keys servers under `servers` (not Cursor's
        // `mcpServers`) and names the transport explicitly.
        let entry = serde_json::json!({ "type": "stdio", "command": "mesh-mcp", "args": [] });
        Self::upsert_mcp_server(
            &cur_dir.join(".vscode").join("mcp.json"),
            "servers",
            entry,
            Some("mcpServers"),
        )
        .map(|written| {
            if written {
                eprintln!("  ✔ Detected VS Code: Added 'mesh-mcp' entry to .vscode/mcp.json");
            }
        })
    }

    /// Adds or replaces only the `mesh-mcp` entry under `root_key` in an IDE's
    /// MCP config, keeping every other server and top-level key the user has.
    /// Earlier versions rewrote the whole file with a fresh object, silently
    /// deleting the user's other MCP servers.
    ///
    /// `legacy_key` names a root key older versions wrongly wrote `mesh-mcp`
    /// under; that stale entry is removed (and the key too, once empty).
    ///
    /// A file that exists but is not a JSON object is left untouched with a
    /// warning — never overwritten. Returns whether the file was written.
    fn upsert_mcp_server(
        path: &Path,
        root_key: &str,
        entry: serde_json::Value,
        legacy_key: Option<&str>,
    ) -> Result<bool, std::io::Error> {
        let mut doc = match fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => serde_json::json!({}),
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) if v.is_object() => v,
                _ => {
                    eprintln!(
                        "  ⚠ {} is not a valid JSON object; left unchanged. Add the 'mesh-mcp' server by hand.",
                        path.display()
                    );
                    return Ok(false);
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(e) => return Err(e),
        };
        let Some(obj) = doc.as_object_mut() else {
            return Ok(false);
        };

        if let Some(legacy) = legacy_key {
            let now_empty = obj
                .get_mut(legacy)
                .and_then(|v| v.as_object_mut())
                .map(|servers| {
                    servers.remove("mesh-mcp");
                    servers.is_empty()
                })
                .unwrap_or(false);
            if now_empty {
                obj.remove(legacy);
            }
        }

        let servers = obj.entry(root_key).or_insert_with(|| serde_json::json!({}));
        if !servers.is_object() {
            eprintln!(
                "  ⚠ '{root_key}' in {} is not an object; left unchanged.",
                path.display()
            );
            return Ok(false);
        }
        if let Some(servers) = servers.as_object_mut() {
            servers.insert("mesh-mcp".to_string(), entry);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(&doc)? + "\n")?;
        Ok(true)
    }
}

/// Opening marker of the section `init --write-ide-config` manages in
/// `CLAUDE.md` / `AGENTS.md`; everything outside the markers is the user's.
const GUIDANCE_BEGIN: &str =
    "<!-- mesh-mcp:begin (managed by `mesh-mcp init --write-ide-config`) -->";
const GUIDANCE_END: &str = "<!-- mesh-mcp:end -->";

/// The routing section, framed as a verifiable starting point rather than an
/// authority: the imperative, "do not verify" wording of 7.0.18 was what the
/// bench agents rejected.
fn agent_guidance(for_claude_code: bool, team_skill: bool) -> String {
    let mut text = format!(
        "{GUIDANCE_BEGIN}
## Cross-repository navigation: MeshMCP

This workspace is indexed by MeshMCP (MCP server `mesh-mcp`). For a question that spans several
repositories, start from the graph, then check what matters with your usual read and search tools:

- who imports or uses a package, module or symbol → `find_dependents`
- events: who publishes, which topics, queues or streams, which consumers → `analyze_impact`
- gRPC: server handlers, client call sites, `.proto` changes → `analyze_grpc`

Every row carries a `path:line`: a verifiable starting point, not an answer to take on trust. Rows
marked `heuristic` or `ambiguous` deserve a check.

For an exact identifier (an enum value, a class name), plain text search is still the right tool.
"
    );
    if for_claude_code {
        text.push_str(
            "
In Claude Code these tools load on demand: `ToolSearch` with
`select:mcp__mesh-mcp__find_dependents,mcp__mesh-mcp__analyze_impact,mcp__mesh-mcp__analyze_grpc`.
",
        );
    }
    if team_skill {
        text.push_str("\nTeam guide: `.agents/skills/mesh-mcp/SKILL.md`.\n");
    }
    text.push_str(GUIDANCE_END);
    text.push('\n');
    text
}

/// Replaces the marked section of `path` with `block`, or appends it (after a
/// blank line) when the file has none; creates the file if missing. A file
/// with only one of the two markers is left untouched with a warning — its
/// section cannot be located safely. Returns whether the file was written.
fn upsert_guidance(path: &Path, block: &str) -> Result<bool, std::io::Error> {
    let current = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let updated = match (current.find(GUIDANCE_BEGIN), current.find(GUIDANCE_END)) {
        (Some(begin), Some(end)) if begin < end => {
            let mut after = end + GUIDANCE_END.len();
            if current[after..].starts_with('\n') {
                after += 1;
            }
            format!("{}{block}{}", &current[..begin], &current[after..])
        }
        (None, None) if current.trim().is_empty() => block.to_string(),
        (None, None) => {
            let separator = if current.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            format!("{current}{separator}{block}")
        }
        _ => {
            eprintln!(
                "  ⚠ {} has an incomplete mesh-mcp section; left unchanged.",
                path.display()
            );
            return Ok(false);
        }
    };
    if updated == current {
        return Ok(false);
    }
    fs::write(path, updated)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `InitCommand::run` reads `std::env::current_dir()` directly (no injectable
    // base dir), and `current_dir` is process-wide state — Rust runs tests in
    // this binary on multiple threads by default, so any two tests that both
    // change cwd would race. Serialize just the cwd-touching tests on this lock;
    // every other test in the crate is unaffected.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("json")
    }

    /// `--write-ide-config` used to overwrite `.cursor/mcp.json` with a fresh
    /// object, deleting every other MCP server the user had configured.
    #[test]
    fn cursor_config_keeps_other_servers_and_keys() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(".cursor").join("mcp.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            r#"{"mcpServers":{"github":{"command":"gh-mcp"},"mesh-mcp":{"command":"old"}},"other":1}"#,
        )
        .expect("write");

        InitCommand::configure_cursor(tmp.path()).expect("configure");
        let doc = read_json(&path);
        assert_eq!(doc["mcpServers"]["github"]["command"], "gh-mcp");
        assert_eq!(doc["mcpServers"]["mesh-mcp"]["command"], "mesh-mcp");
        assert_eq!(doc["other"], 1);

        // Idempotent: a second run changes nothing.
        let before = fs::read_to_string(&path).expect("read");
        InitCommand::configure_cursor(tmp.path()).expect("configure again");
        assert_eq!(fs::read_to_string(&path).expect("read"), before);
    }

    /// VS Code keys servers under `servers`; the stale `mcpServers.mesh-mcp`
    /// older versions wrote there is removed, the user's own entries are kept.
    #[test]
    fn vscode_config_uses_servers_key_and_drops_legacy_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(".vscode").join("mcp.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            r#"{"servers":{"db":{"type":"stdio","command":"db-mcp"}},"mcpServers":{"mesh-mcp":{"command":"mesh-mcp","args":[]}}}"#,
        )
        .expect("write");

        InitCommand::configure_vscode(tmp.path()).expect("configure");
        let doc = read_json(&path);
        assert_eq!(doc["servers"]["db"]["command"], "db-mcp");
        assert_eq!(doc["servers"]["mesh-mcp"]["type"], "stdio");
        assert!(doc.get("mcpServers").is_none(), "{doc}");
    }

    /// A config that is not a JSON object is never overwritten.
    #[test]
    fn malformed_ide_config_is_left_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(".cursor").join("mcp.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let broken = "{ \"mcpServers\": { oops";
        fs::write(&path, broken).expect("write");

        InitCommand::configure_cursor(tmp.path()).expect("no hard failure");
        assert_eq!(fs::read_to_string(&path).expect("read"), broken);
    }

    /// Claude Code's project config gets the same merge (7.0.0 printed
    /// "Registered Claude Code CLI guidance" and wrote nothing).
    #[test]
    fn claude_code_project_config_is_merged() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(".mcp.json");
        fs::write(&path, r#"{"mcpServers":{"github":{"command":"gh-mcp"}}}"#).expect("write");
        InitCommand::configure_claude_code(tmp.path()).expect("configure");
        let doc = read_json(&path);
        assert_eq!(doc["mcpServers"]["github"]["command"], "gh-mcp");
        assert_eq!(doc["mcpServers"]["mesh-mcp"]["command"], "mesh-mcp");
        assert_eq!(doc["mcpServers"]["mesh-mcp"]["type"], "stdio");
    }

    /// An existing config is the user's: kept byte-for-byte unless `--force`.
    #[test]
    fn existing_config_is_kept_unless_forced() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original_cwd = std::env::current_dir().expect("cwd");
        let temp = tempfile::tempdir().expect("tempdir");
        let config = temp.path().join(".agents/mesh-mcp.toml");
        fs::create_dir_all(config.parent().expect("parent")).expect("mkdir");
        let tuned = "[workspace]\nname = \"tuned\"\nversion = \"0\"\nroots = [\"../a\"]\n";
        fs::write(&config, tuned).expect("write");

        std::env::set_current_dir(temp.path()).expect("chdir");
        let kept = InitCommand::run(true, false, false);
        let kept_text = fs::read_to_string(&config).expect("read");
        let forced = InitCommand::run(true, false, true);
        let forced_text = fs::read_to_string(&config).expect("read");
        std::env::set_current_dir(&original_cwd).expect("restore cwd");

        kept.expect("init without --force");
        forced.expect("init --force");
        assert_eq!(kept_text, tuned);
        assert!(
            forced_text.contains("Generated by 'mesh-mcp init --auto'"),
            "{forced_text}"
        );
    }

    /// No config yet: one is created with just our entry.
    #[test]
    fn missing_ide_config_is_created() {
        let tmp = tempfile::tempdir().expect("tempdir");
        InitCommand::configure_cursor(tmp.path()).expect("configure");
        let doc = read_json(&tmp.path().join(".cursor").join("mcp.json"));
        assert_eq!(doc["mcpServers"]["mesh-mcp"]["command"], "mesh-mcp");
    }

    /// Runs `InitCommand::run(true, false, false)` inside a fresh temp dir containing
    /// `marker_files`, restores the original cwd afterward, and returns the
    /// generated `roots` list from `.agents/mesh-mcp.toml`.
    fn roots_for(marker_files: &[&str]) -> Vec<String> {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original_cwd = std::env::current_dir().expect("cwd");
        let temp = tempfile::tempdir().expect("tempdir");

        for marker in marker_files {
            let path = temp.path().join(marker);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("mkdir marker parent");
            }
            fs::write(path, "").expect("write marker file");
        }

        std::env::set_current_dir(temp.path()).expect("chdir into tempdir");
        let result = InitCommand::run(true, false, false);
        std::env::set_current_dir(&original_cwd).expect("restore cwd");
        result.expect("InitCommand::run should succeed");

        let toml = fs::read_to_string(temp.path().join(".agents/mesh-mcp.toml"))
            .expect("generated config should exist");
        let roots_line = toml
            .lines()
            .find(|l| l.starts_with("roots = "))
            .expect("config should have a roots line");
        roots_line
            .trim_start_matches("roots = [")
            .trim_end_matches(']')
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    #[test]
    fn go_module_roots_the_whole_repo() {
        let roots = roots_for(&["go.mod"]);
        assert!(
            roots.contains(&"..".to_string()),
            "a bare go.mod (no docs/, no services/, ...) must still produce a \
             root covering the repo's Go source, got: {roots:?}"
        );
    }

    #[test]
    fn python_project_roots_the_whole_repo() {
        let roots = roots_for(&["pyproject.toml"]);
        assert!(
            roots.contains(&"..".to_string()),
            "a bare pyproject.toml must still produce a root covering the \
             repo's Python source, got: {roots:?}"
        );
    }

    #[test]
    fn java_gradle_project_roots_the_whole_repo() {
        let roots = roots_for(&["build.gradle"]);
        assert!(
            roots.contains(&"..".to_string()),
            "a bare build.gradle must still produce a root covering the \
             repo's Java source, got: {roots:?}"
        );
    }

    #[test]
    fn single_crate_rust_project_roots_the_whole_repo() {
        // Cargo.toml present but no crates/ subdir — the "else if" branch.
        let roots = roots_for(&["Cargo.toml"]);
        assert!(
            roots.contains(&"..".to_string()),
            "a single-crate Cargo.toml (no crates/) must still produce a root \
             covering the repo's Rust source, got: {roots:?}"
        );
    }

    #[test]
    fn single_package_node_project_roots_the_whole_repo() {
        // package.json present but no packages/ subdir — the "else if" branch.
        let roots = roots_for(&["package.json"]);
        assert!(
            roots.contains(&"..".to_string()),
            "a single-package package.json (no packages/) must still produce a \
             root covering the repo's JS/TS source, got: {roots:?}"
        );
    }

    #[test]
    fn go_module_with_docs_still_roots_the_whole_repo() {
        // A docs/ dir populates `roots` before the go.mod check runs, so the
        // whole-repo "." root for go.mod is pushed unconditionally.
        let roots = roots_for(&["go.mod", "docs/architecture.md"]);
        assert!(
            roots.contains(&"..".to_string()),
            "go.mod's source root must not be silently dropped just because \
             docs/ also matched, got: {roots:?}"
        );
    }

    #[test]
    fn multiple_whole_repo_markers_do_not_duplicate_the_root() {
        // A repo with both Cargo.toml and pyproject.toml (no crates/ or
        // packages/ subdir) would otherwise push "." twice.
        let roots = roots_for(&["Cargo.toml", "pyproject.toml"]);
        let dot_dot_count = roots.iter().filter(|r| *r == "..").count();
        assert_eq!(
            dot_dot_count, 1,
            "duplicate whole-repo roots should be deduped, got: {roots:?}"
        );
    }

    #[test]
    fn polyglot_src_container_with_no_root_markers_is_detected() {
        // Services whose language markers live one level under `src/`
        // (`src/checkout/go.mod`, `src/email/pyproject.toml`) rather than
        // at the repo root are detected as a glob root.
        let roots = roots_for(&[
            "src/checkoutservice/go.mod",
            "src/emailservice/pyproject.toml",
            "docs/architecture.md",
        ]);
        assert!(
            roots.contains(&"../src/*".to_string()),
            "a src/ container with >= 2 nested per-service language markers \
             must produce a glob root covering every service, got: {roots:?}"
        );
    }

    #[test]
    fn ordinary_src_dir_with_a_single_project_is_not_treated_as_a_container() {
        // A normal single-project repo's `src/` holds source files directly,
        // not nested sub-projects — it must not get a spurious "../src/*"
        // root on top of the already-correct whole-repo root.
        let roots = roots_for(&["Cargo.toml", "src/main.rs"]);
        assert!(
            !roots.iter().any(|r| r == "../src/*"),
            "an ordinary src/ with no nested language markers must not be \
             treated as a polyglot service container, got: {roots:?}"
        );
    }

    /// A repo with a plain `gateway/` directory (no `api-gateway/`) used to
    /// still get the literal, nonexistent root `./api-gateway`, which could
    /// never match anything once the config was loaded.
    #[test]
    fn plain_gateway_dir_roots_itself_not_the_nonexistent_api_gateway_path() {
        let roots = roots_for(&["gateway/main.go"]);
        assert!(
            roots.contains(&"../gateway".to_string()),
            "a plain gateway/ dir must root itself, got: {roots:?}"
        );
        assert!(
            !roots.contains(&"../api-gateway".to_string()),
            "must not root the nonexistent api-gateway path, got: {roots:?}"
        );
    }

    #[test]
    fn api_gateway_dir_is_still_detected_when_it_exists() {
        let roots = roots_for(&["api-gateway/main.go"]);
        assert!(
            roots.contains(&"../api-gateway".to_string()),
            "an actual api-gateway/ dir must still be rooted, got: {roots:?}"
        );
    }

    /// Same literal-path bug as the gateway case, for the docs/architecture
    /// detection: a repo with only `architecture/` (no `docs/`) used to get
    /// the nonexistent root `./docs`.
    #[test]
    fn plain_architecture_dir_roots_itself_not_the_nonexistent_docs_path() {
        let roots = roots_for(&["architecture/overview.md"]);
        assert!(
            roots.contains(&"../architecture".to_string()),
            "an architecture/ dir with no docs/ must root itself, got: {roots:?}"
        );
        assert!(
            !roots.contains(&"../docs".to_string()),
            "must not root the nonexistent docs path, got: {roots:?}"
        );
    }

    /// `protos/` (plural) must be detected the same way `proto/` and
    /// `proto-registry/` already are.
    #[test]
    fn protos_plural_dir_is_detected_as_a_proto_root() {
        let roots = roots_for(&["protos/billing.proto"]);
        assert!(
            roots.contains(&"../proto*".to_string()),
            "a protos/ (plural) dir must be detected as a proto root, got: {roots:?}"
        );
    }

    #[test]
    fn monorepo_workspace_configuration_roots_the_whole_repo() {
        let roots = roots_for(&["pnpm-workspace.yaml", "packages/frontend/package.json"]);
        assert!(
            roots.contains(&"..".to_string()),
            "a monorepo with pnpm-workspace.yaml must root the workspace root '..', got: {roots:?}"
        );
        assert!(
            roots.contains(&"../packages/*".to_string()),
            "a monorepo with packages/ must also root '../packages/*', got: {roots:?}"
        );
    }

    /// 7.0.19: the routing section lands in both files, is idempotent, and
    /// never touches what the user wrote around it.
    #[test]
    fn agent_guidance_is_upserted_between_markers_and_keeps_user_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let claude = tmp.path().join("CLAUDE.md");
        fs::write(&claude, "# Team notes\n\nKeep this.\n").expect("seed");

        InitCommand::write_agent_guidance(tmp.path()).expect("first write");
        let first = fs::read_to_string(&claude).expect("read");
        assert!(
            first.starts_with("# Team notes\n\nKeep this.\n\n"),
            "{first}"
        );
        assert!(
            first.contains("ToolSearch"),
            "Claude Code gets the loading line"
        );
        let agents = fs::read_to_string(tmp.path().join("AGENTS.md")).expect("AGENTS.md created");
        assert!(agents.contains("find_dependents") && !agents.contains("ToolSearch"));

        fs::write(&claude, first.clone() + "\nAfter the section.\n").expect("user edit");
        InitCommand::write_agent_guidance(tmp.path()).expect("second write");
        let second = fs::read_to_string(&claude).expect("read");
        assert_eq!(
            second,
            first.clone() + "\nAfter the section.\n",
            "idempotent"
        );
        assert_eq!(second.matches(GUIDANCE_BEGIN).count(), 1);
    }

    #[test]
    fn agent_guidance_replaces_an_older_section_and_names_the_team_skill() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skill = tmp.path().join(".agents").join("skills").join("mesh-mcp");
        fs::create_dir_all(&skill).expect("mkdir");
        fs::write(skill.join("SKILL.md"), "---\nname: mesh-mcp\n---\n").expect("skill");
        let agents = tmp.path().join("AGENTS.md");
        fs::write(
            &agents,
            format!("intro\n{GUIDANCE_BEGIN}\nold text\n{GUIDANCE_END}\noutro\n"),
        )
        .expect("seed");

        InitCommand::write_agent_guidance(tmp.path()).expect("write");
        let text = fs::read_to_string(&agents).expect("read");
        assert!(
            text.starts_with("intro\n") && text.ends_with("outro\n"),
            "{text}"
        );
        assert!(!text.contains("old text"));
        assert!(text.contains(".agents/skills/mesh-mcp/SKILL.md"));
    }

    #[test]
    fn half_marked_guidance_file_is_left_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("CLAUDE.md");
        let text = format!("{GUIDANCE_BEGIN}\nno end marker\n");
        fs::write(&path, &text).expect("seed");
        assert!(!upsert_guidance(&path, "new\n").expect("upsert"));
        assert_eq!(fs::read_to_string(&path).expect("read"), text);
    }
}
