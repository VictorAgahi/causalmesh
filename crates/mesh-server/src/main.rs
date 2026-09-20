use clap::{Parser, Subcommand};
use mesh_core::{
    expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config, FilesystemCrawler,
    ValidatedScope,
};
use mesh_parsers::PolyglotIndexer;
use mesh_server::cli::{DoctorCommand, HooksCommand, InitCommand};
use mesh_server::run_server;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(
    name = "mesh-mcp",
    version = "2.9.0",
    about = "Universal Polyglot Architecture Mesh & Contract Governance MCP Server"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long, help = "Path to custom configuration file")]
    config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the MeshMCP JSON-RPC server over stdio (default)
    Run,

    /// Run diagnostic healthchecks on environment, permissions, and roots
    Doctor,

    /// Automatically scan polyglot workspace and generate .agents/mesh-mcp.toml
    Init {
        #[arg(
            long,
            help = "Automatically detect all services and schemas without prompts"
        )]
        auto: bool,

        #[arg(
            long,
            help = "Generate IDE configurations for Cursor, VS Code, and Claude Code"
        )]
        write_ide_config: bool,
    },

    /// Install OS-level Git pre-commit hooks for active governance
    InstallHooks,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Commandment 3: Route all tracing strictly to stderr so stdout is reserved for JSON-RPC MCP frames
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Run) {
        Commands::Doctor => {
            DoctorCommand::run(cli.config.as_deref())?;
        }
        Commands::Init {
            auto,
            write_ide_config,
        } => {
            InitCommand::run(auto, write_ide_config)?;
        }
        Commands::InstallHooks => {
            HooksCommand::run()?;
        }
        Commands::Run => {
            let config_paths = [
                cli.config.clone(),
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
                let default_cfg_str = r#"
[workspace]
name = "default-mesh"
version = "2.9.0"
roots = ["."]
"#;
                (Config::load_from_str(default_cfg_str)?, PathBuf::from("."))
            };

            let allowed_roots = match expand_roots(&config.workspace.roots, &base_dir) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(target: "mesh::config", "Failed to expand roots: {e}. Falling back to base directory.");
                    vec![dunce::canonicalize(&base_dir).unwrap_or(base_dir.clone())]
                }
            };

            let audit = Arc::new(AuditLogger::new(None)?);
            let rescan = Arc::new(BackgroundRescanEngine::new()?);

            let state = Arc::new(AppState::new(
                config.clone(),
                allowed_roots.clone(),
                audit,
                rescan,
            ));

            // Initial Ingestion Phase
            for root in &allowed_roots {
                if let Ok(validated_scope) =
                    ValidatedScope::resolve(&root.to_string_lossy(), &allowed_roots)
                {
                    let files = FilesystemCrawler::crawl_scope(
                        &validated_scope,
                        &config.workspace.exclude_patterns,
                        Some(10),
                    );
                    let mut graph = (*state.contract_graph.load().as_ref()).clone();
                    let mut doc_index = (*state.doc_index.load().as_ref()).clone();
                    let mut prop_reg = (*state.property_registry.load().as_ref()).clone();

                    for file in files {
                        if let Ok(content) = std::fs::read_to_string(&file) {
                            let path_str = file.to_string_lossy();
                            if path_str.ends_with(".md") {
                                doc_index.index_markdown_file(&file, &content);
                            } else if path_str.ends_with(".properties") {
                                prop_reg.ingest_properties_str(&content);
                            } else if path_str.ends_with(".yml") || path_str.ends_with(".yaml") {
                                let _ = prop_reg.ingest_yaml_str(&content);
                                PolyglotIndexer::index_file(&file, &content, 0, &mut graph);
                            } else {
                                PolyglotIndexer::index_file(&file, &content, 0, &mut graph);
                            }
                        }
                    }

                    state.contract_graph.store(Arc::new(graph));
                    state.doc_index.store(Arc::new(doc_index));
                    state.property_registry.store(Arc::new(prop_reg));
                }
            }

            let cancel_token = CancellationToken::new();
            let cancel_sig = cancel_token.clone();

            // Graceful shutdown on SIGINT / Ctrl-C per RFC Section 3.6
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!(target: "mesh::shutdown", "SIGINT/Ctrl-C received. Initiating graceful shutdown.");
                cancel_sig.cancel();
            });

            run_server(state, cancel_token).await?;
        }
    }

    Ok(())
}
