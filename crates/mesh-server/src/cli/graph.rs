use mesh_core::{expand_roots, Config, ContractGraph, FilesystemCrawler, ValidatedScope};
use mesh_parsers::{GraphRenderer, PolyglotIndexer};
use std::path::{Path, PathBuf};

pub struct GraphCommand;

impl GraphCommand {
    pub fn run(
        config_path: Option<&Path>,
        format: &str,
        output_path: Option<&Path>,
        open_browser: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("Indexing Polyglot Architecture Mesh Topology...");

        let config_paths = [
            config_path.map(|p| p.to_path_buf()),
            Some(PathBuf::from(".agents/mesh-mcp.toml")),
            Some(PathBuf::from("mesh-mcp.toml")),
        ];

        let found_path = config_paths.into_iter().flatten().find(|p| p.exists());
        let (config, base_dir) = if let Some(ref path) = found_path {
            let cfg = Config::load_from_file(path)?;
            let base = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            (cfg, base)
        } else {
            let default =
                "[workspace]\nname = \"default-mesh\"\nversion = \"2.9.0\"\nroots = [\".\"]\n";
            (Config::load_from_str(default)?, PathBuf::from("."))
        };

        let allowed_roots = match expand_roots(&config.workspace.roots, &base_dir) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(target: "mesh::graph", "Failed to expand roots: {e}");
                vec![dunce::canonicalize(&base_dir).unwrap_or(base_dir)]
            }
        };

        let mut graph = ContractGraph::new();
        let mut file_count = 0usize;

        for root in &allowed_roots {
            if let Ok(validated_scope) =
                ValidatedScope::resolve(&root.to_string_lossy(), &allowed_roots)
            {
                let files = FilesystemCrawler::crawl_scope(
                    &validated_scope,
                    &config.workspace.exclude_patterns,
                    Some(10),
                );

                for file in files {
                    if let Ok(content) = std::fs::read_to_string(&file) {
                        file_count += 1;
                        let path_str = file.to_string_lossy();
                        if !path_str.ends_with(".md") && !path_str.ends_with(".properties") {
                            PolyglotIndexer::index_file(&file, &content, 0, &mut graph);
                            if let Some(ref contracts_cfg) = config.engines.contracts {
                                PolyglotIndexer::apply_custom_patterns(
                                    &file,
                                    &content,
                                    0,
                                    &contracts_cfg.patterns,
                                    &mut graph,
                                );
                            }
                        }
                    }
                }
            }
        }

        graph.reconcile_edges();

        eprintln!(
            "✔ Scanned {file_count} files across {} roots: {} contracts/nodes, {} causal links/edges.",
            allowed_roots.len(),
            graph.node_count(),
            graph.edge_count()
        );

        let workspace_name = &config.workspace.name;
        let rendered = match format.to_lowercase().as_str() {
            "mermaid" => GraphRenderer::to_mermaid(&graph, workspace_name),
            "json" => GraphRenderer::to_json(&graph, workspace_name),
            _ => GraphRenderer::to_html(&graph, workspace_name),
        };

        let final_path = if let Some(out) = output_path {
            std::fs::write(out, &rendered)?;
            eprintln!("✔ Saved topology visualization to: {}", out.display());
            Some(out.to_path_buf())
        } else if format.eq_ignore_ascii_case("html") {
            let cache_dir = dirs_or_temp_cache().join("mesh");
            std::fs::create_dir_all(&cache_dir)?;
            let temp_html = cache_dir.join("topology.html");
            std::fs::write(&temp_html, &rendered)?;
            eprintln!("✔ Generated HTML topology: {}", temp_html.display());
            Some(temp_html)
        } else {
            // Print directly to stdout for mermaid or json pipes
            println!("{rendered}");
            None
        };

        if open_browser {
            if let Some(target_file) = final_path {
                eprintln!("🚀 Opening topology in your default browser...");
                open_in_browser(&target_file);
            }
        }

        Ok(())
    }
}

fn dirs_or_temp_cache() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        PathBuf::from(home).join(".cache")
    } else {
        std::env::temp_dir()
    }
}

fn open_in_browser(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();

    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();

    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", &path.to_string_lossy()])
        .spawn();
}
