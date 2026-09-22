use crate::indexer::WorkspaceIndexer;
use mesh_parsers::GraphRenderer;
use std::io::Write;
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

        let (config, base_dir) = WorkspaceIndexer::discover_config(config_path)?;
        let allowed_roots = WorkspaceIndexer::resolve_roots(&config, &base_dir);
        let (graph, file_count) = WorkspaceIndexer::build_graph(&config, &allowed_roots);

        eprintln!(
            "✔ Scanned {file_count} files across {} roots: {} contracts/nodes, {} causal links/edges.",
            allowed_roots.len(),
            graph.node_count(),
            graph.edge_count()
        );

        let workspace_name = &config.workspace.name;
        let repo_names = WorkspaceIndexer::repo_names(&allowed_roots);
        let rendered = match format.to_lowercase().as_str() {
            "mermaid" => GraphRenderer::to_mermaid(&graph, workspace_name, &repo_names),
            "json" => GraphRenderer::to_json(&graph, workspace_name, &repo_names),
            _ => GraphRenderer::to_html(&graph, workspace_name, &repo_names),
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
            // Commandment 3 exemption: this is the `graph` CLI subcommand's own output,
            // not the JSON-RPC stdio stream owned by StdioFramingActor. The `graph`
            // subcommand never runs the JSON-RPC loop, so writing rendered output
            // directly to stdout here cannot corrupt a live frame. Written through an
            // explicit io::stdout() handle (rather than println!) so this exemption is
            // visible at grep level.
            let mut stdout = std::io::stdout();
            writeln!(stdout, "{rendered}")?;
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
