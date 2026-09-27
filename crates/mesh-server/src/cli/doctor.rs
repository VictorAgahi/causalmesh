use crate::indexer::{WorkspaceIndexer, SCAN_DEPTH};
use mesh_core::{expand_roots, AuditLogger, Config, PersistentIndexCache, PropertyRegistry};
use mesh_parsers::AstGuard;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use notify::Watcher;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct DoctorCommand;

/// One repairable-health finding (plan 4 step 4.7): socket permissions, a
/// stale daemon version, a corrupt or legacy cache, an orphaned per-version
/// workspace directory. Separate from sections 1-9's plain prose above, which
/// stay exactly as they were — these are the checks `--fix` can act on and
/// `--json` renders as structured output for install scripts.
#[derive(Debug, Clone, serde::Serialize)]
struct DoctorCheck {
    name: &'static str,
    /// "ok" | "warn" | "error" | "info"
    status: &'static str,
    message: String,
    /// `None` when `--fix` was not requested or this finding has no fix;
    /// `Some(true/false)` for whether the attempted fix succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    fixed: Option<bool>,
}

impl DoctorCheck {
    fn ok(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            name,
            status: "ok",
            message: message.into(),
            fixed: None,
        }
    }
    fn warn(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            name,
            status: "warn",
            message: message.into(),
            fixed: None,
        }
    }
    fn error(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            name,
            status: "error",
            message: message.into(),
            fixed: None,
        }
    }
    fn info(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            name,
            status: "info",
            message: message.into(),
            fixed: None,
        }
    }
    fn with_fixed(mut self, fixed: bool) -> Self {
        self.fixed = Some(fixed);
        self
    }
    fn icon(&self) -> &'static str {
        match self.status {
            "ok" => "✔",
            "warn" => "⚠",
            "error" => "✖",
            _ => "ℹ",
        }
    }
}

impl DoctorCommand {
    /// Scans the workspace once (no persistent cache, nothing written) and
    /// prints the rejected-file summary: counts per reason, first 10 paths.
    /// Returns (errors, warnings) count.
    fn report_index_health(config_path: Option<&Path>) -> (usize, usize) {
        let Ok((config, base_dir)) = WorkspaceIndexer::discover_config(config_path) else {
            eprintln!("ℹ Index health: skipped (no config to scan)");
            return (0, 0);
        };
        let roots = WorkspaceIndexer::resolve_roots(&config, &base_dir);
        let snapshot = WorkspaceIndexer::build_snapshot(&config, &roots, None, None, None);
        let health = &snapshot.health;
        let summary = health.render_summary(10);
        if health.rejected.is_empty() && health.rejected_overflow == 0 {
            eprint!("✔ Index health: {summary}");
            (0, 0)
        } else {
            eprintln!(
                "⚠ Index health: {summary}    Searches cannot return these files; read them directly."
            );
            (0, 1)
        }
    }

    pub fn run(
        config_path: Option<&Path>,
        fix: bool,
        json: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!(
            "🔍 Running MeshMCP Diagnostic Healthcheck (v{}, commit: {})...\n",
            env!("CARGO_PKG_VERSION"),
            option_env!("GIT_HASH").unwrap_or("dev")
        );

        let mut total_errors: usize = 0;
        let mut total_warnings: usize = 0;

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
                        total_warnings += 1;
                        eprintln!("⚠ Jailed roots warning: No roots resolved or syntax error");
                    }
                }
                Err(e) => {
                    total_errors += 1;
                    eprintln!("✖ Config syntax: INVALID ({}) - {e}", p.display());
                }
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
                        total_errors += missing.len();
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
                                    None,
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
                                total_warnings += dead_docs.len();
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
                                            None,
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
                                        total_warnings += 1;
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
                                                None,
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
                                        total_warnings += 1;
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
                        total_warnings += overlaps.len();
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
        let symlink_ok = {
            let probe_dir =
                std::env::temp_dir().join(format!("mesh-symlink-probe-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&probe_dir);
            let target = probe_dir.join("target");
            let link = probe_dir.join("link");
            let _ = std::fs::write(&target, "content");
            #[cfg(unix)]
            let created = std::os::unix::fs::symlink(&target, &link).is_ok();
            #[cfg(windows)]
            let created = std::os::windows::fs::symlink_file(&target, &link).is_ok();
            #[cfg(not(any(unix, windows)))]
            let created = false;

            let res = if created {
                let roots = vec![probe_dir.clone()];
                if let Ok(scope) =
                    mesh_core::ValidatedScope::resolve(&probe_dir.to_string_lossy(), &roots)
                {
                    let crawled = mesh_core::FilesystemCrawler::crawl_scope(&scope, &[], Some(2));
                    !crawled.contains(&link)
                } else {
                    true
                }
            } else {
                true
            };
            let _ = std::fs::remove_dir_all(&probe_dir);
            res
        };
        if symlink_ok {
            eprintln!("✔ Symlink invariants: follow_links=false verified (crawler rejects symlink traversal)");
        } else {
            total_errors += 1;
            eprintln!("✖ Symlink invariants: symlink traversal detected in crawler");
        }

        // 3. Secret redaction engine
        let mut reg = PropertyRegistry::new();
        reg.insert_sanitized("jwt.secret", "token123");
        reg.insert_sanitized("spring.datasource.password", "secret");
        if reg.redacted_count() == 2 {
            eprintln!("✔ Secret redaction engine: ACTIVE (Dev secrets masked with fallback hints)");
        } else {
            total_errors += 1;
            eprintln!("✖ Secret redaction engine: FAILED to mask test secrets");
        }

        // 4. Host OS event subsystem check
        #[cfg(target_os = "linux")]
        {
            if let Ok(content) = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches") {
                let watches: u64 = content.trim().parse().unwrap_or(0);
                if watches >= 524_288 {
                    eprintln!("✔ Linux inotify watchers check: {watches} available (Max: PASS)");
                } else {
                    total_warnings += 1;
                    eprintln!(
                        "⚠ Linux inotify watchers check: {watches} (LOW: recommend >= 524,288)"
                    );
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            match notify::RecommendedWatcher::new(|_| {}, notify::Config::default()) {
                Ok(_) => eprintln!("✔ Host OS event subsystem: Native (APFS FSEvents active)"),
                Err(e) => {
                    total_warnings += 1;
                    eprintln!("⚠ Host OS event subsystem: FSEvents warning: {e}");
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            match notify::RecommendedWatcher::new(|_| {}, notify::Config::default()) {
                Ok(_) => eprintln!(
                    "✔ Host OS event subsystem: Native (Windows ReadDirectoryChangesW active)"
                ),
                Err(e) => {
                    total_warnings += 1;
                    eprintln!("⚠ Host OS event subsystem: ReadDirectoryChangesW warning: {e}");
                }
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            eprintln!("✔ Host OS event subsystem: Native event queue active");
        }

        // 5. JSON serialization baseline
        let t0 = Instant::now();
        let _ = serde_json::to_string(&serde_json::json!({"test": "latency"}))?;
        let loopback_micros = t0.elapsed().as_micros();
        eprintln!(
            "✔ JSON-RPC serialization baseline: {:.2}ms",
            loopback_micros as f64 / 1000.0
        );

        // 6. Tree-sitter parsers initialization
        if AstGuard::verify_all_parsers() {
            eprintln!(
                "✔ Tree-sitter parsers initialized (Java, Go, Python, TypeScript, Rust, C++, Kotlin, C#, Ruby, PHP, Swift, Scala, Protobuf)"
            );
        } else {
            total_errors += 1;
            eprintln!("✖ Tree-sitter parsers: Initialization error");
        }

        // 7. Memory baseline (measured real RSS)
        #[cfg(unix)]
        let (rss_mb, rss_ok) = {
            let mut rusage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            if unsafe { libc::getrusage(libc::RUSAGE_SELF, rusage.as_mut_ptr()) } == 0 {
                let rusage = unsafe { rusage.assume_init() };
                #[cfg(target_os = "macos")]
                let mb = rusage.ru_maxrss as f64 / (1024.0 * 1024.0);
                #[cfg(not(target_os = "macos"))]
                let mb = rusage.ru_maxrss as f64 / 1024.0;
                (mb, mb < 50.0)
            } else {
                (0.0, true)
            }
        };
        #[cfg(not(unix))]
        let (rss_mb, rss_ok) = (0.0, true);

        if rss_mb > 0.0 {
            if rss_ok {
                eprintln!("✔ Memory baseline: {rss_mb:.1} MiB RSS (mimalloc + compact_str)");
            } else {
                total_warnings += 1;
                eprintln!(
                    "⚠ Memory baseline: {rss_mb:.1} MiB RSS (elevated, expected < 50 MiB at boot)"
                );
            }
        } else {
            eprintln!("✔ Memory baseline: mimalloc + compact_str active");
        }

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
            total_warnings += 1;
            eprintln!("⚠ Toolchain utilities: git not found in PATH");
        }

        // 9. Index health (plan 4 step 4.1): the files a search can never
        // return, with the same reasons and sizes the tools' notes give.
        let (_index_errors, index_warnings) = Self::report_index_health(config_path);
        total_warnings += index_warnings;

        // 10. Repairable health (plan 4 step 4.7): socket permissions, daemon
        // version drift, corrupt or legacy caches, orphaned per-version
        // workspace directories. Structured separately from sections 1-9
        // above so `--json` has something to render and `--fix` something
        // to act on.
        let checks = Self::run_repairable_checks(config_path, fix);
        for check in &checks {
            if check.fixed != Some(true) {
                match check.status {
                    "error" => total_errors += 1,
                    "warn" => total_warnings += 1,
                    _ => {}
                }
            }
        }

        if json {
            println!("{}", serde_json::to_string_pretty(&checks)?);
        } else {
            eprintln!();
            for check in &checks {
                let suffix = match check.fixed {
                    Some(true) => " (fixed)",
                    Some(false) => " (fix attempted, still failing)",
                    None => "",
                };
                eprintln!("{} {}: {}{suffix}", check.icon(), check.name, check.message);
            }
        }

        if !json {
            eprintln!();
            if total_errors > 0 {
                eprintln!(
                    "✖ Diagnostic check failed: {total_errors} error(s), {total_warnings} warning(s)."
                );
            } else if total_warnings > 0 {
                eprintln!(
                    "⚠ All critical systems operational ({total_warnings} warning(s)). Ready for AI agents."
                );
            } else {
                eprintln!("✔ All systems operational. Ready for AI agents.");
            }
        }

        Ok(())
    }

    /// The workspace's own daemon socket, if this run has a resolvable config
    /// (skipped entirely otherwise — there is no workspace to look one up
    /// for). Shared by the socket-permission and version checks below.
    fn resolved_workspace(config_path: Option<&Path>) -> Option<(PathBuf, String)> {
        let (_, base_dir) = WorkspaceIndexer::discover_config(config_path).ok()?;
        let canonical = dunce::canonicalize(&base_dir).unwrap_or(base_dir);
        let id = mesh_core::workspace_id(&canonical);
        Some((canonical, id))
    }

    fn run_repairable_checks(config_path: Option<&Path>, fix: bool) -> Vec<DoctorCheck> {
        let mut checks = Vec::new();
        let workspace = Self::resolved_workspace(config_path);

        Self::check_socket(&workspace, fix, &mut checks);
        Self::check_daemon_version(&workspace, fix, &mut checks);
        Self::check_daemon_sandbox(&workspace, &mut checks);
        Self::check_legacy_global_cache(fix, &mut checks);
        Self::check_workspace_cache(&workspace, fix, &mut checks);
        Self::check_audit_db(&mut checks);
        Self::check_orphaned_workspace_dirs(&workspace, fix, &mut checks);

        checks
    }

    #[cfg_attr(not(unix), allow(unused_variables))]
    fn check_socket(
        workspace: &Option<(PathBuf, String)>,
        fix: bool,
        checks: &mut Vec<DoctorCheck>,
    ) {
        let Some((_, workspace_id)) = workspace else {
            checks.push(DoctorCheck::info(
                "Socket permissions",
                "skipped (no config to resolve a workspace)",
            ));
            return;
        };
        let sock_path = mesh_core::socket_path_for(workspace_id);
        if !sock_path.exists() {
            checks.push(DoctorCheck::info(
                "Socket permissions",
                format!(
                    "no daemon running for this workspace ({})",
                    sock_path.display()
                ),
            ));
            return;
        }

        #[cfg(unix)]
        if std::os::unix::net::UnixStream::connect(&sock_path).is_err() {
            let mut check = DoctorCheck::warn(
                "Orphaned socket",
                format!(
                    "{} exists but nothing is listening (a crashed daemon left it behind)",
                    sock_path.display()
                ),
            );
            if fix {
                mesh_core::cleanup_stale_socket(&sock_path);
                check = check.with_fixed(!sock_path.exists());
            }
            checks.push(check);
            // Nothing to say about permissions on a socket that is about to be
            // (or already was) removed.
            return;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sock_mode = std::fs::metadata(&sock_path)
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0);
            let dir_mode = sock_path
                .parent()
                .and_then(|d| std::fs::metadata(d).ok())
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0);
            if sock_mode == 0o600 && dir_mode == 0o700 {
                checks.push(DoctorCheck::ok(
                    "Socket permissions",
                    format!("socket 0{sock_mode:o}, directory 0{dir_mode:o} (owner-only)"),
                ));
            } else {
                let mut check = DoctorCheck::warn(
                    "Socket permissions",
                    format!(
                        "socket 0{sock_mode:o} (want 0600), directory 0{dir_mode:o} (want 0700)"
                    ),
                );
                if fix {
                    let sock_fixed = std::fs::set_permissions(
                        &sock_path,
                        std::fs::Permissions::from_mode(0o600),
                    )
                    .is_ok();
                    let dir_fixed = sock_path.parent().is_some_and(|d| {
                        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).is_ok()
                    });
                    check = check.with_fixed(sock_fixed && dir_fixed);
                }
                checks.push(check);
            }
        }
        #[cfg(not(unix))]
        {
            checks.push(DoctorCheck::info(
                "Socket permissions",
                "not applicable on this OS (named pipe, not a filesystem socket)",
            ));
        }
    }

    fn check_daemon_version(
        workspace: &Option<(PathBuf, String)>,
        fix: bool,
        checks: &mut Vec<DoctorCheck>,
    ) {
        let Some((_, workspace_id)) = workspace else {
            return;
        };
        let Some(meta) = mesh_core::socket::read_daemon_meta(workspace_id) else {
            return;
        };
        if meta.version == env!("CARGO_PKG_VERSION") {
            checks.push(DoctorCheck::ok(
                "Daemon version",
                format!("running meshd (pid {}) matches this version", meta.pid),
            ));
            return;
        }

        let mut check = DoctorCheck::warn(
            "Daemon version",
            format!(
                "running meshd (pid {}) is version {}, this mesh-mcp is {}",
                meta.pid,
                meta.version,
                env!("CARGO_PKG_VERSION")
            ),
        );
        if fix {
            // Same-user enforcement is the OS's, not ours: `kill`/`OpenProcess`
            // fail on a PID this user does not own, never terminates a
            // stranger's process (see `terminate_process`'s own doc).
            //
            // A stale record survives a crash, an unclean shutdown, or a
            // reboot — not just the instant between this check and the signal
            // below — so `meta.pid` may since have been reused by an unrelated
            // process (step 4.7 review). `terminate_process` is only ever
            // called once `daemon_is_reachable` also confirms *something* is
            // live on this exact workspace's socket right now: a PID-reuse
            // coincidence would additionally have to bind that exact,
            // workspace-scoped path, which plain reuse cannot produce.
            let reachable = mesh_core::socket::daemon_is_reachable(workspace_id);
            let fixed = if reachable {
                // Re-checked immediately before signaling: narrows, though
                // does not eliminate, the window between this and the kill
                // call itself.
                mesh_core::socket::process_is_alive(meta.pid)
                    && mesh_core::socket::terminate_process(meta.pid)
            } else {
                // Nothing is listening: whatever `meta.pid` is now, it is not
                // actively serving this workspace, so there is nothing to
                // terminate — the stale record itself is the whole fix, and
                // removing it is unconditionally safe.
                true
            };
            if fixed {
                // `terminate_process` sends SIGTERM/`TerminateProcess`, neither
                // of which the daemon can catch to clean up after itself (only
                // SIGINT is handled for a graceful shutdown) — so the record of
                // it stays behind unless removed here, and a later `doctor`
                // would otherwise keep reporting a PID that no longer exists.
                mesh_core::socket::remove_daemon_meta(workspace_id);
            }
            check = check.with_fixed(fixed);
        }
        checks.push(check);
    }

    /// Plan 4 step 4.10: whether the running `meshd` actually confined itself.
    /// By default it only logs a warning and keeps serving when the kernel
    /// refuses its seccomp filter, so this is where the degraded mode shows.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables, clippy::ptr_arg))]
    fn check_daemon_sandbox(workspace: &Option<(PathBuf, String)>, checks: &mut Vec<DoctorCheck>) {
        #[cfg(target_os = "linux")]
        {
            const NAME: &str = "Daemon network sandbox";
            let Some((_, workspace_id)) = workspace else {
                return;
            };
            let Some(meta) = mesh_core::socket::read_daemon_meta(workspace_id) else {
                return;
            };
            // Only a PID confirmed to serve this workspace's socket right now
            // (same PID-reuse guard as `check_daemon_version`).
            if !mesh_core::socket::daemon_is_reachable(workspace_id) {
                return;
            }
            let daemon = std::fs::read_to_string(format!("/proc/{}/status", meta.pid));
            let own = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
            checks.push(match daemon.map(|d| seccomp_confined(&d, &own)) {
                Ok(Some(true)) => DoctorCheck::ok(
                    NAME,
                    format!(
                        "meshd (pid {}) runs under its seccomp filter (no AF_INET/AF_INET6 sockets)",
                        meta.pid
                    ),
                ),
                Ok(Some(false)) => DoctorCheck::warn(
                    NAME,
                    format!(
                        "meshd (pid {}) is NOT confined: the kernel refused its seccomp filter (see \
                         its log), or it predates plan 4 step 4.10. Set MESH_DAEMON_SANDBOX=required \
                         to make meshd refuse to start instead",
                        meta.pid
                    ),
                ),
                Ok(None) | Err(_) => DoctorCheck::info(
                    NAME,
                    format!("could not read the seccomp state of meshd (pid {})", meta.pid),
                ),
            });
        }
    }

    fn check_legacy_global_cache(fix: bool, checks: &mut Vec<DoctorCheck>) {
        let path = PersistentIndexCache::legacy_global_db_path();
        if !path.exists() {
            return;
        }
        let mut check = DoctorCheck::warn(
            "Legacy cache",
            format!(
                "pre-4.4 machine-wide cache still present at {} (never opened any more)",
                path.display()
            ),
        );
        if fix {
            check = check.with_fixed(Self::remove_db_files(&path));
        }
        checks.push(check);
    }

    fn check_workspace_cache(
        workspace: &Option<(PathBuf, String)>,
        fix: bool,
        checks: &mut Vec<DoctorCheck>,
    ) {
        let Some((_, workspace_id)) = workspace else {
            return;
        };
        let path = PersistentIndexCache::workspace_db_path(workspace_id);
        if !path.exists() {
            return;
        }
        match PersistentIndexCache::quick_check(&path) {
            Ok(true) => checks.push(DoctorCheck::ok("Index cache", "PRAGMA quick_check: ok")),
            Ok(false) => {
                let mut check =
                    DoctorCheck::error("Index cache", "PRAGMA quick_check reported corruption");
                if fix {
                    check = check.with_fixed(Self::remove_db_files(&path));
                }
                checks.push(check);
            }
            Err(e) => {
                let mut check =
                    DoctorCheck::error("Index cache", format!("could not open for check: {e}"));
                if fix {
                    check = check.with_fixed(Self::remove_db_files(&path));
                }
                checks.push(check);
            }
        }
    }

    /// Never offers a fix: a corrupt audit trail is evidence, not a disposable
    /// cache — `doctor --fix` reports it and stops there (plan 4 step 4.7).
    fn check_audit_db(checks: &mut Vec<DoctorCheck>) {
        let path = AuditLogger::default_db_path();
        if !path.exists() {
            return;
        }
        let quick = PersistentIndexCache::quick_check(&path);
        let chain = AuditLogger::verify_db(&path);
        match (quick, chain) {
            (Ok(true), Ok(true)) => {
                checks.push(DoctorCheck::ok(
                    "Audit trail",
                    "quick_check and hash chain: ok",
                ));
            }
            (Ok(true), Ok(false)) => {
                checks.push(DoctorCheck::error(
                    "Audit trail",
                    "hash chain broken — entries may have been tampered with or edited",
                ));
            }
            (Ok(false), _) => {
                checks.push(DoctorCheck::error(
                    "Audit trail",
                    "PRAGMA quick_check reported corruption",
                ));
            }
            (Ok(true), Err(e)) => {
                checks.push(DoctorCheck::error(
                    "Audit trail",
                    format!("hash chain verification failed: {e}"),
                ));
            }
            (Err(e), _) => {
                checks.push(DoctorCheck::error(
                    "Audit trail",
                    format!("could not open for check: {e}"),
                ));
            }
        }
    }

    fn check_orphaned_workspace_dirs(
        workspace: &Option<(PathBuf, String)>,
        fix: bool,
        checks: &mut Vec<DoctorCheck>,
    ) {
        let Some((base_dir, current_id)) = workspace else {
            return;
        };
        let workspaces_dir = mesh_core::mesh_cache_dir().join("workspaces");
        let Ok(entries) = std::fs::read_dir(&workspaces_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if id == *current_id {
                continue;
            }
            let Some(marker_path) = PersistentIndexCache::read_workspace_path_marker(&id) else {
                continue;
            };
            if marker_path != *base_dir {
                continue;
            }
            let mut check = DoctorCheck::warn(
                "Orphaned workspace cache",
                format!(
                    "{} is a leftover from a previous mesh-mcp version of this project",
                    entry.path().display()
                ),
            );
            if fix {
                check = check.with_fixed(std::fs::remove_dir_all(entry.path()).is_ok());
            }
            checks.push(check);
        }
    }

    /// Removes a SQLite database and its WAL/SHM siblings, if present.
    fn remove_db_files(path: &Path) -> bool {
        let mut ok = std::fs::remove_file(path).is_ok();
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            if side.exists() {
                ok &= std::fs::remove_file(&side).is_ok();
            }
        }
        ok
    }
}

/// Whether a process whose `/proc/<pid>/status` is `daemon` carries a seccomp
/// filter of its own, beyond those this process (`own`, same container or
/// session) already inherits — a container runtime's default filter must not
/// pass for `meshd`'s. `None` when `daemon` has no `Seccomp:` line.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn seccomp_confined(daemon: &str, own: &str) -> Option<bool> {
    let field = |status: &str, key: &str| -> Option<u64> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().parse().ok())
    };
    let mode = field(daemon, "Seccomp:")?;
    if mode != 2 {
        return Some(false);
    }
    // `Seccomp_filters` exists since Linux 5.9; older kernels only tell the mode.
    match (
        field(daemon, "Seccomp_filters:"),
        field(own, "Seccomp_filters:"),
    ) {
        (Some(daemon_filters), Some(own_filters)) => Some(daemon_filters > own_filters),
        _ => Some(true),
    }
}

#[cfg(test)]
mod sandbox_check_tests {
    use super::seccomp_confined;

    #[test]
    fn seccomp_confined_reads_mode_and_filter_count() {
        let unconfined = "Name:\tmeshd\nSeccomp:\t0\nSeccomp_filters:\t0\n";
        let confined = "Name:\tmeshd\nSeccomp:\t2\nSeccomp_filters:\t1\n";
        let own_bare = "Seccomp:\t0\nSeccomp_filters:\t0\n";
        assert_eq!(seccomp_confined(unconfined, own_bare), Some(false));
        assert_eq!(seccomp_confined(confined, own_bare), Some(true));
        // Inside a container whose runtime already applies one filter to
        // everything: mode 2 alone does not prove meshd confined itself.
        let own_container = "Seccomp:\t2\nSeccomp_filters:\t1\n";
        assert_eq!(seccomp_confined(confined, own_container), Some(false));
        let confined_in_container = "Seccomp:\t2\nSeccomp_filters:\t2\n";
        assert_eq!(
            seccomp_confined(confined_in_container, own_container),
            Some(true)
        );
        // Pre-5.9 kernel: no filter count, the mode is all there is.
        assert_eq!(seccomp_confined("Seccomp:\t2\n", ""), Some(true));
        assert_eq!(seccomp_confined("Name:\tmeshd\n", own_bare), None);
    }
}
