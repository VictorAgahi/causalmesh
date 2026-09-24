use std::fs;
use std::path::Path;

pub struct InitCommand;

impl InitCommand {
    pub fn run(_auto: bool, write_ide_config: bool) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("🔍 Scanning workspace tree for polyglot services and schemas...");

        let mut roots = Vec::new();
        let cur_dir = std::env::current_dir()?;

        // Scan for common project types
        if cur_dir.join("proto-registry").exists() || cur_dir.join("proto").exists() {
            eprintln!("  Found: Protobuf schemas");
            roots.push("./proto*".to_string());
        }

        if cur_dir.join("api-gateway").exists() || cur_dir.join("gateway").exists() {
            eprintln!("  Found: API Gateway");
            roots.push("./api-gateway".to_string());
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

        if cur_dir.join("packages").exists() {
            eprintln!("  Found: Monorepo packages (./packages/*)");
            roots.push("./packages/*".to_string());
        } else if cur_dir.join("package.json").exists() {
            eprintln!("  Found: Node / TypeScript project (package.json)");
            // No packages/ subdir — same reasoning as the Cargo.toml-only case above.
            roots.push(".".to_string());
        }

        // Container directories (`src/`, `apps/`) holding multiple per-service
        // projects, each with its own language marker one level down — the
        // `services`/`packages` checks above only catch those exact directory
        // names; this catches the same shape under the other common names
        // (e.g. Google's "Online Boutique" reference microservices repo uses
        // `src/checkoutservice/go.mod`, `src/emailservice/pyproject.toml`, ...,
        // never a root-level marker). Require >= 2 matching subdirectories so
        // an ordinary single-project `src/` (plain source files, no nested
        // sub-projects) doesn't get a redundant extra root.
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

        if cur_dir.join("docs").exists() || cur_dir.join("architecture").exists() {
            eprintln!("  Found: Architecture docs");
            roots.push("./docs".to_string());
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
            if let Ok(entries) = fs::read_dir(&cur_dir) {
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

        // Determine stop_rules dynamically
        let mut stop_rules = Vec::new();
        if cur_dir.join("proto-registry").exists() || cur_dir.join("proto").exists() {
            stop_rules.push("\"proto-registry\" = \"🛑 STOP CASCADE CI : proto-registry generates TS/Go/Java stubs. Do not modify microservices directly!\"".to_string());
        }
        if cur_dir.join("k8s-infrastructure").exists()
            || cur_dir.join("deploy").exists()
            || cur_dir.join("k8s").exists()
        {
            stop_rules.push("\"k8s-infrastructure\" = \"🛑 STOP INFRASTRUCTURE : K8s manifest modification requires DevOps review!\"".to_string());
        }

        let stop_rules_section = if stop_rules.is_empty() {
            "".to_string()
        } else {
            format!("\n[engines.policy.stop_rules]\n{}\n", stop_rules.join("\n"))
        };

        let toml_content = format!(
            r#"# mesh-mcp.toml — Generated by 'mesh-mcp init --auto'
[workspace]
name = "enterprise-polyglot-mesh"
version = "2.9.0"
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

        if write_ide_config {
            eprintln!("\n🔌 Auto-Configuring IDEs (--write-ide-config):");
            Self::configure_cursor(&cur_dir)?;
            Self::configure_vscode(&cur_dir)?;
            eprintln!("  ✔ Registered Claude Code CLI guidance");
        }

        eprintln!("\n🚀 Zero setup left. Ready for AI agents!");
        Ok(())
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
        let cursor_dir = cur_dir.join(".cursor");
        fs::create_dir_all(&cursor_dir)?;
        let mcp_path = cursor_dir.join("mcp.json");

        let content = serde_json::json!({
            "mcpServers": {
                "mesh-mcp": {
                    "command": "mesh-mcp",
                    "args": []
                }
            }
        });

        fs::write(mcp_path, serde_json::to_string_pretty(&content)?)?;
        eprintln!("  ✔ Detected Cursor: Added 'mesh-mcp' entry to .cursor/mcp.json");
        Ok(())
    }

    fn configure_vscode(cur_dir: &Path) -> Result<(), std::io::Error> {
        let vscode_dir = cur_dir.join(".vscode");
        fs::create_dir_all(&vscode_dir)?;
        let mcp_path = vscode_dir.join("mcp.json");

        let content = serde_json::json!({
            "mcpServers": {
                "mesh-mcp": {
                    "command": "mesh-mcp",
                    "args": []
                }
            }
        });

        fs::write(mcp_path, serde_json::to_string_pretty(&content)?)?;
        eprintln!("  ✔ Detected VS Code: Added 'mesh-mcp' entry to .vscode/mcp.json");
        Ok(())
    }
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

    /// Runs `InitCommand::run(true, false)` inside a fresh temp dir containing
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
        let result = InitCommand::run(true, false);
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
        // Regression test for the exact bug found on the kubernetes benchmark:
        // a docs/ dir populates `roots` before the go.mod check runs, so the
        // `roots.is_empty()` fallback never fires and the whole-repo "." root
        // for go.mod must be pushed unconditionally by its own block.
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
        // Regression test for the exact bug found benchmarking against
        // Google's "Online Boutique" reference microservices repo: every
        // service's language marker lives one level under `src/`
        // (`src/checkoutservice/go.mod`, `src/emailservice/pyproject.toml`),
        // never at the repo root, so every root-level marker check misses
        // and (pre-fix) only `roots = ["../docs"]` was emitted.
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
}
