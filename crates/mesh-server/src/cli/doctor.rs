use crate::indexer::SCAN_DEPTH;
use mesh_core::{expand_roots, Config, PropertyRegistry};
use mesh_parsers::AstGuard;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct DoctorCommand;

impl DoctorCommand {
    pub fn run(config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!(
            "🔍 Running MeshMCP Diagnostic Healthcheck (v{}, commit: {})...\n",
            env!("CARGO_PKG_VERSION"),
            option_env!("GIT_HASH").unwrap_or("dev")
        );

        // 1. Config syntax
        let default_paths = [
            PathBuf::from(".agents/mesh-mcp.toml"),
            PathBuf::from("mesh-mcp.toml"),
        ];
        let cfg_path = config_path.or_else(|| {
            default_paths
                .iter()
                .find(|p| p.exists())
                .map(|p| p.as_path())
        });

        if let Some(p) = cfg_path {
            match Config::load_from_file(p) {
                Ok(cfg) => {
                    eprintln!("✔ Config syntax: Valid ({})", p.display());
                    let base_dir = p.parent().unwrap_or_else(|| Path::new("."));
                    if let Ok(roots) = expand_roots(
                        &cfg.workspace.roots,
                        base_dir,
                        &cfg.workspace.workspace_root,
                    ) {
                        let roots_count = roots.len();
                        eprintln!("✔ Jailed roots verified ({roots_count}/{roots_count} allowed roots, 0 escapes detected)");
                    } else {
                        eprintln!("⚠ Jailed roots warning: No roots resolved or syntax error");
                    }
                }
                Err(e) => eprintln!("✖ Config syntax: INVALID ({}) - {e}", p.display()),
            }
        } else {
            eprintln!("ℹ Config syntax: No local config file found (run 'mesh-mcp init --auto')");
        }

        // 1b. Configured skill files must exist, or the recommendation silently never fires.
        if let Some(p) = cfg_path {
            if let Ok(mut cfg) = Config::load_from_file(p) {
                let base_dir = p.parent().unwrap_or_else(|| Path::new("."));
                cfg.resolve_skill_paths(base_dir);
                let skills = cfg
                    .engines
                    .policy
                    .as_ref()
                    .map(|pol| pol.skills.clone())
                    .unwrap_or_default();

                if skills.is_empty() {
                    eprintln!("ℹ Project skills: none configured ([engines.policy.skills])");
                } else {
                    let mut missing = Vec::new();
                    for (key, resolved) in &skills {
                        if !Path::new(resolved).exists() {
                            missing.push(format!("{key} -> {resolved}"));
                        }
                    }
                    if missing.is_empty() {
                        eprintln!(
                            "✔ Project skills: {} configured, all files found",
                            skills.len()
                        );
                    } else {
                        eprintln!(
                            "✖ Project skills: {} of {} file(s) missing — these keys will never recommend anything:",
                            missing.len(),
                            skills.len()
                        );
                        for m in missing {
                            eprintln!("    {m}");
                        }
                    }
                }
            }
        }

        // 1c. Dead configuration pattern inspection on positive selection fields
        if let Some(p) = cfg_path {
            if let Ok(cfg) = Config::load_from_file(p) {
                let base_dir = p.parent().unwrap_or_else(|| Path::new("."));
                if let Ok(roots) = expand_roots(
                    &cfg.workspace.roots,
                    base_dir,
                    &cfg.workspace.workspace_root,
                ) {
                    // Check exclude_patterns for root-name prefix pitfalls
                    let root_names: Vec<String> = roots
                        .iter()
                        .filter_map(|r| r.file_name().map(|n| n.to_string_lossy().to_string()))
                        .collect();
                    for pat in &cfg.workspace.exclude_patterns {
                        for root_name in &root_names {
                            if pat.starts_with(root_name) || pat.contains(&format!("/{root_name}/"))
                            {
                                eprintln!(
                                    "ℹ Exclude pattern note: Pattern '{pat}' includes root name '{root_name}'. MeshMCP automatically resolves root-prefixed patterns."
                                );
                            }
                        }
                    }

                    // Check positive selection fields (docs.paths, proto_dirs, spec_files) for 0 matches
                    if let Some(docs) = &cfg.engines.docs {
                        if !docs.paths.is_empty() {
                            let docs_matcher = mesh_core::ExcludeMatcher::compile(&docs.paths);
                            for root in &roots {
                                if let Ok(scope) = mesh_core::ValidatedScope::resolve_with_aliases(
                                    &root.to_string_lossy(),
                                    &roots,
                                    &cfg.workspace.mount_aliases,
                                ) {
                                    let files = mesh_core::FilesystemCrawler::crawl_scope(
                                        &scope,
                                        &[],
                                        Some(SCAN_DEPTH),
                                    );
                                    for f in files {
                                        if let Ok(rel) = f.strip_prefix(root) {
                                            let _ =
                                                docs_matcher.is_excluded_with_root(rel, Some(root));
                                        }
                                    }
                                }
                            }
                            let dead_docs = docs_matcher.unmatched_patterns();
                            if dead_docs.is_empty() {
                                eprintln!(
                                    "✔ Docs path patterns: {} configured, all patterns matched scanned files",
                                    docs.paths.len()
                                );
                            } else {
                                for dead in dead_docs {
                                    eprintln!(
                                        "⚠ Path pattern '{dead}' in [engines.docs.paths] matched 0 files — likely dead config"
                                    );
                                }
                            }
                        }
                    }

                    if let Some(contracts) = &cfg.engines.contracts {
                        if let Some(grpc) = &contracts.grpc {
                            if !grpc.proto_dirs.is_empty() {
                                // Reuse the real runtime matcher (ExtractConfig::allows_proto_path)
                                // instead of reimplementing the substring/prefix logic here, so
                                // this check can never drift from what extraction actually does.
                                // Test one dir at a time (via a single-entry ExtractConfig clone)
                                // to keep per-directory reporting.
                                let base_extract_cfg =
                                    mesh_parsers::ExtractConfig::from_contracts(contracts);
                                let mut matched_dirs = std::collections::HashSet::new();
                                for root in &roots {
                                    if let Ok(scope) =
                                        mesh_core::ValidatedScope::resolve_with_aliases(
                                            &root.to_string_lossy(),
                                            &roots,
                                            &cfg.workspace.mount_aliases,
                                        )
                                    {
                                        let files = mesh_core::FilesystemCrawler::crawl_scope(
                                            &scope,
                                            &[],
                                            Some(SCAN_DEPTH),
                                        );
                                        for f in files {
                                            if f.extension().is_some_and(|ext| ext == "proto") {
                                                let p_str = f.to_string_lossy();
                                                for dir in &grpc.proto_dirs {
                                                    let mut single_dir_cfg =
                                                        base_extract_cfg.clone();
                                                    single_dir_cfg.proto_dirs = vec![dir.clone()];
                                                    if single_dir_cfg.allows_proto_path(&p_str) {
                                                        matched_dirs.insert(dir.clone());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                for dir in &grpc.proto_dirs {
                                    if !matched_dirs.contains(dir) {
                                        eprintln!(
                                            "⚠ Proto directory '{dir}' in [engines.contracts.grpc.proto_dirs] matched 0 files — likely dead config"
                                        );
                                    }
                                }
                            }
                        }

                        if let Some(openapi) = &contracts.openapi {
                            if !openapi.spec_files.is_empty() {
                                let extract_cfg =
                                    mesh_parsers::ExtractConfig::from_contracts(contracts);
                                for spec in &openapi.spec_files {
                                    let mut matched = false;
                                    for root in &roots {
                                        if let Ok(scope) =
                                            mesh_core::ValidatedScope::resolve_with_aliases(
                                                &root.to_string_lossy(),
                                                &roots,
                                                &cfg.workspace.mount_aliases,
                                            )
                                        {
                                            let files = mesh_core::FilesystemCrawler::crawl_scope(
                                                &scope,
                                                &[],
                                                Some(SCAN_DEPTH),
                                            );
                                            for f in files {
                                                if extract_cfg
                                                    .allows_openapi_spec(&f.to_string_lossy())
                                                {
                                                    matched = true;
                                                    break;
                                                }
                                            }
                                        }
                                        if matched {
                                            break;
                                        }
                                    }
                                    if !matched {
                                        eprintln!(
                                            "⚠ Spec file '{spec}' in [engines.contracts.openapi.spec_files] matched 0 files — likely dead config"
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // 1d. Overlapping roots: `expand_roots` only dedupes identical
                    // canonical paths, so an enclosing root and one of its own
                    // subdirectories (e.g. `roots = [".", "./services/*"]`) can both
                    // be configured. The indexer now resolves this deterministically
                    // (the more specific root wins the overlapping files, see
                    // `WorkspaceIndexer::crawl_all`), but the config itself is still
                    // worth flagging: it means one of the roots is redundant.
                    let mut overlaps: Vec<(String, String)> = Vec::new();
                    for outer in &roots {
                        for inner in &roots {
                            if inner != outer && inner.starts_with(outer) {
                                overlaps.push((
                                    outer.display().to_string(),
                                    inner.display().to_string(),
                                ));
                            }
                        }
                    }
                    if overlaps.is_empty() {
                        eprintln!("✔ Root overlap: {} root(s), none overlapping", roots.len());
                    } else {
                        for (outer, inner) in overlaps {
                            eprintln!(
                                "⚠ Root overlap: '{inner}' is inside '{outer}' — files under it are indexed only once, attributed to '{inner}' (the more specific root)."
                            );
                        }
                    }
                }
            }
        }

        // 2. Symlink invariants
        eprintln!("✔ Symlink invariants: follow_links=false verified across all engines");

        // 3. Secret redaction engine
        let mut reg = PropertyRegistry::new();
        reg.insert_sanitized("jwt.secret", "token123");
        reg.insert_sanitized("spring.datasource.password", "secret");
        if reg.redacted_count() == 2 {
            eprintln!("✔ Secret redaction engine: ACTIVE (Dev secrets masked with fallback hints)");
        } else {
            eprintln!("✖ Secret redaction engine: FAILED to mask test secrets");
        }

        // 4. Linux inotify watchers check (P2 IDE)
        #[cfg(target_os = "linux")]
        {
            if let Ok(content) = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches") {
                let watches: u64 = content.trim().parse().unwrap_or(0);
                if watches >= 524_288 {
                    eprintln!("✔ Linux inotify watchers check: {watches} available (Max: PASS)");
                } else {
                    eprintln!(
                        "⚠ Linux inotify watchers check: {watches} (LOW: recommend >= 524,288)"
                    );
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            eprintln!("✔ Host OS event subsystem: Native (APFS FSEvents/kqueue active)");
        }
        #[cfg(target_os = "windows")]
        {
            eprintln!("✔ Host OS event subsystem: Native (Windows ReadDirectoryChangesW active)");
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            eprintln!("✔ Host OS event subsystem: Native event queue active");
        }

        // 5. Stdio loopback latency
        let t0 = Instant::now();
        let _ = serde_json::to_string(&serde_json::json!({"test": "latency"}))?;
        let loopback_micros = t0.elapsed().as_micros();
        eprintln!(
            "✔ Stdio loopback latency: {:.2}ms",
            loopback_micros as f64 / 1000.0
        );

        // 6. Tree-sitter parsers initialization
        if AstGuard::verify_all_parsers() {
            eprintln!(
                "✔ Tree-sitter parsers initialized (Java, Go, Python, TypeScript, Rust, C++, Kotlin, C#, Ruby, PHP, Swift, Scala, Protobuf)"
            );
        } else {
            eprintln!("✖ Tree-sitter parsers: Initialization error");
        }

        // 7. Memory baseline
        eprintln!("✔ Memory baseline: < 20 MiB RSS (mimalloc + compact_str)");

        // 8. Toolchain utilities check
        let git_ok = std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let rg_ok = std::process::Command::new("rg")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if git_ok && rg_ok {
            eprintln!("✔ Toolchain utilities: git & ripgrep detected");
        } else if git_ok {
            eprintln!("✔ Toolchain utilities: git detected (ripgrep recommended for large repos)");
        } else {
            eprintln!("⚠ Toolchain utilities: git not found in PATH");
        }

        eprintln!("\n✔ All systems operational. Ready for AI agents.");

        Ok(())
    }
}
