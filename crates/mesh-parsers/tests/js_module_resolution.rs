#![allow(clippy::unwrap_used, clippy::expect_used)]
//! tsconfig / jsconfig `paths` and package.json `imports` aliases, end to end:
//! an aliased import becomes an `Imports` edge to the declaration it names.

use mesh_core::{ContractGraph, EdgeKind};
use mesh_parsers::{ExtractConfig, PolyglotIndexer, TsAliasIndex};
use std::path::{Path, PathBuf};

fn write(root: &Path, rel: &str, content: &str) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, content).expect("write");
    path
}

fn import_edges(root: &Path, files: &[(&str, &str)], aliases: bool) -> Vec<(String, String)> {
    let cfg = ExtractConfig {
        ts_aliases: if aliases {
            TsAliasIndex::new(std::slice::from_ref(&root.to_path_buf()))
        } else {
            TsAliasIndex::default()
        },
        ..ExtractConfig::default()
    };
    let mut graph = ContractGraph::new();
    for (rel, code) in files {
        let path = write(root, rel, code);
        PolyglotIndexer::extract_with_config(&path, code, 0, &cfg).apply(&mut graph);
    }
    graph.reconcile_edges();
    let mut edges: Vec<(String, String)> = graph
        .all_edges()
        .iter()
        .filter(|e| e.kind == EdgeKind::Imports)
        .filter_map(|e| {
            Some((
                graph.get_node(e.from)?.name.to_string(),
                graph.get_node(e.to)?.name.to_string(),
            ))
        })
        .collect();
    edges.sort();
    edges
}

fn root() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize");
    (tmp, root)
}

const TSCONFIG: &str =
    r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["src/*"] } } }"#;

#[test]
fn tsconfig_alias_import_becomes_an_edge() {
    let (_t, root) = root();
    write(&root, "tsconfig.json", TSCONFIG);
    let files = [
        ("src/services/User.ts", "export default class User {}\n"),
        (
            "src/app/main.ts",
            "import User from '@/services/User';\nexport class Main { u: User }\n",
        ),
    ];
    assert_eq!(
        import_edges(&root, &files, true),
        vec![("Main".to_string(), "User".to_string())]
    );
    // A default import names no symbol: without alias resolution the module
    // specifier alone cannot be resolved.
    assert!(import_edges(&root, &files, false).is_empty());
}

#[test]
fn package_imports_alias_in_javascript_becomes_an_edge() {
    let (_t, root) = root();
    write(
        &root,
        "package.json",
        r##"{ "imports": { "#lib/*": "./lib/*.js" } }"##,
    );
    let files = [
        ("lib/logger.js", "export default class {}\n"),
        (
            "app/server.js",
            "import Logger from '#lib/logger';\nexport class Server { l = new Logger(); }\n",
        ),
    ];
    assert_eq!(
        import_edges(&root, &files, true),
        vec![("Server".to_string(), "logger".to_string())]
    );
}

#[test]
fn require_with_an_alias_is_rewritten_too() {
    let (_t, root) = root();
    write(&root, "jsconfig.json", TSCONFIG);
    let files = [
        ("src/services/Billing.js", "module.exports = class {};\n"),
        (
            "src/app/run.js",
            "const Billing = require('@/services/Billing');\nexports.run = function run() { return new Billing(); };\n",
        ),
    ];
    let edges = import_edges(&root, &files, true);
    assert!(
        edges
            .iter()
            .any(|(from, to)| from == "run" && to == "Billing"),
        "{edges:?}"
    );
}

#[test]
fn a_package_that_shares_an_alias_prefix_is_not_rewritten() {
    let (_t, root) = root();
    write(
        &root,
        "tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@app": ["src/app.ts"] } } }"#,
    );
    let files = [
        ("src/app.ts", "export class App {}\n"),
        (
            "src/other.ts",
            "import { x } from '@application/core';\nexport class Other {}\n",
        ),
    ];
    assert!(import_edges(&root, &files, true).is_empty());
}
