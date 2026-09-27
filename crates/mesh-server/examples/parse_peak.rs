//! Per-file parse memory probe for plan 4.9 (not part of the shipped binary).
//!
//! `cargo run --release -p mesh-server --example parse_peak -- <mode> <file>`
//! under `/usr/bin/time -l` reads the "peak memory footprint" of one parser on
//! one file. Every mode first reads the file into memory exactly like
//! `WorkspaceIndexer::process_file` does (`std::fs::read` + UTF-8 check), so
//! `peak(<mode>) - peak(read)` is the parser's own cost on top of the file bytes
//! the indexer already holds.
//!
//! Modes:
//! - `read`:  only the read (the baseline);
//! - `props`: `PropertyRegistry::ingest_yaml_str` (every `.yml`/`.yaml` by default);
//! - `spec`:  `PolyglotIndexer::extract_with_config` (OpenAPI/AsyncAPI shapes);
//! - `docs`:  `DocIndex::parse_sections` with the default doc settings.
//!
//! Results are kept alive until exit so the peak includes what the indexer would
//! keep resident, and a one-line summary goes to stderr (stdout stays unused).

use mesh_core::{DocIndex, PropertyRegistry};
use mesh_parsers::{ExtractConfig, PolyglotIndexer};
use mimalloc::MiMalloc;
use std::path::Path;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(mode), Some(file)) = (args.next(), args.next()) else {
        return Err("usage: parse_peak <read|props|spec|docs> <file>".into());
    };
    let path = Path::new(&file);
    let bytes = std::fs::read(path)?;
    let content = std::str::from_utf8(&bytes)?;

    let summary = match mode.as_str() {
        "read" => format!("read {} bytes", content.len()),
        "props" => {
            let mut reg = PropertyRegistry::with_redaction(true);
            let res = reg.ingest_yaml_str(content);
            let s = format!("props: {} keys, result {:?}", reg.len(), res.err());
            std::hint::black_box(&reg);
            s
        }
        "spec" => {
            let idx =
                PolyglotIndexer::extract_with_config(path, content, 0, &ExtractConfig::default());
            let s = format!(
                "spec: {} nodes, {} producers",
                idx.nodes.len(),
                idx.producers.len()
            );
            std::hint::black_box(&idx);
            s
        }
        "docs" => {
            let sections = DocIndex::default().parse_sections(path, content);
            let s = format!("docs: {} sections", sections.len());
            std::hint::black_box(&sections);
            s
        }
        other => return Err(format!("unknown mode {other}").into()),
    };
    std::hint::black_box(&bytes);
    eprintln!("{summary}");
    Ok(())
}
