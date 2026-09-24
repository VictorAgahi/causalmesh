#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Determinism and idempotence contract of the index (P0 invariants I1–I4).
//!
//! - I1: same input ⇒ same snapshot, whatever the thread count or file order.
//! - I2: an incremental reload ends in the same snapshot as a full rebuild.
//! - I3: reconciling an already reconciled graph changes nothing.
//! - I4: a file reachable from two overlapping roots is indexed once.
//!
//! A test marked `#[ignore = "P0 step 1.x: …"]` documents a violation that
//! still exists; the step that fixes it deletes the attribute. Run everything,
//! ignored tests included, with:
//! `cargo test -p mesh-server --test determinism -- --include-ignored`

use mesh_core::{
    expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config, MeshSnapshot, RepoId,
    SnapshotFingerprint,
};
use mesh_server::WorkspaceIndexer;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ── Fixtures & helpers ──────────────────────────────────────────────────────

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn determinism_fixture() -> PathBuf {
    manifest_dir().join("tests/fixtures/determinism")
}

/// The dedicated fixture plus the two example workspaces shipped in the repo.
fn fixtures() -> Vec<PathBuf> {
    vec![
        determinism_fixture(),
        manifest_dir().join("../../examples/polyglot-shop"),
        manifest_dir().join("../../examples/volontariapp-fixture"),
    ]
}

/// Copies `src` into a fresh temp dir, so tests can mutate it and so the
/// repository's `.gitignore` never applies to the fixture.
fn workspace_copy(src: &Path) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = dunce::canonicalize(tmp.path())
        .expect("canonicalize")
        .join("ws");
    copy_dir(src, &root);
    (tmp, root)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("mkdir");
    for entry in std::fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

fn config(roots: &[&str]) -> Config {
    let list = roots
        .iter()
        .map(|r| format!("\"{r}\""))
        .collect::<Vec<_>>()
        .join(", ");
    Config::load_from_str(&format!(
        "[workspace]\nname = \"determinism\"\nversion = \"0\"\nroots = [{list}]\n"
    ))
    .expect("config")
}

fn resolved_roots(config: &Config, base: &Path) -> Vec<PathBuf> {
    expand_roots(
        &config.workspace.roots,
        base,
        &config.workspace.workspace_root,
    )
    .expect("expand roots")
}

/// Full build over an explicit file list, on a dedicated Rayon pool.
fn build_on_pool(
    config: &Config,
    roots: &[PathBuf],
    files: &[(RepoId, PathBuf)],
    threads: usize,
) -> MeshSnapshot {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("rayon pool")
        .install(|| WorkspaceIndexer::build_snapshot_from_files(config, roots, files, None, None))
}

fn full_fingerprint(config: &Config, roots: &[PathBuf]) -> SnapshotFingerprint {
    WorkspaceIndexer::build_snapshot(config, roots, None, None).fingerprint()
}

/// An `AppState` whose snapshot and VFS come from a full build, ready for
/// incremental `WorkspaceIndexer::reload` calls.
fn indexed_state(config: Config, roots: Vec<PathBuf>) -> Arc<AppState> {
    let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
    let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
    let state = Arc::new(AppState::new(config, roots, audit, rescan));
    let snapshot = {
        let mut vfs = state.vfs.lock().expect("vfs lock");
        WorkspaceIndexer::build_snapshot(&state.config, &state.allowed_roots, None, Some(&mut vfs))
    };
    state.install_snapshot(snapshot);
    state
}

/// Fixed-seed PRNG (64-bit LCG): reproducible shuffles and edit sequences
/// without a test-only dependency.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

// ── Workspace mutations for the incremental tests ──────────────────────────

/// One edit of the determinism fixture. Every write appends a marker whose
/// length grows with `step`, so a file's size differs from all its previous
/// versions: the VFS stat fast path can then never mistake a rewrite for an
/// unchanged file, even on filesystems with coarse mtime granularity.
fn apply_edit(base: &Path, op: usize, step: usize) {
    let marker = "x".repeat(step + 1);
    let write = |rel: &str, content: String| {
        let path = base.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, content).expect("write");
    };
    let toggle = |rel: &str, content: String| {
        let path = base.join(rel);
        if path.exists() {
            std::fs::remove_file(path).expect("remove");
        } else {
            write(rel, content);
        }
    };

    match op % 6 {
        // Change a topic on the producer side.
        0 => write(
            "services/checkout/main.go",
            format!(
                "package main\n\nimport (\n\t\"context\"\n\n\tpb \"example.com/checkout/genproto/shop/v1\"\n\tusersv1 \"example.com/checkout/genproto/users/v1\"\n)\n\ntype checkoutServer struct{{}}\n\nfunc (s *checkoutServer) PlaceOrder(ctx context.Context, req *pb.PlaceOrderRequest) (*pb.PlaceOrderResponse, error) {{\n\treturn nil, nil\n}}\n\nfunc main() {{\n\tpb.RegisterCheckoutServiceServer(grpcServer, &checkoutServer{{}})\n\tadmin := usersv1.NewAdminServiceClient(conn)\n\t_ = admin\n}}\n\nfunc publishOrder(ctx context.Context, w *kafka.Writer) {{\n\tw.WriteMessages(ctx, kafka.Message{{Topic: \"orders.v{step}\"}})\n}}\n\n// {marker}\n"
            ),
        ),
        // Re-index the *target* of an import that an unchanged file resolves.
        1 => write(
            "services/billing/src/main/java/com/acme/shared/Money.java",
            format!(
                "package com.acme.shared;\n\npublic class Money {{\n    public static Money zero() {{\n        return new Money();\n    }}\n\n    public long units{step}() {{\n        return {step};\n    }}\n}}\n// {marker}\n"
            ),
        ),
        // Change a property another file's placeholder refers to.
        2 => write(
            "services/billing/src/main/resources/application-base.properties",
            format!("app.kafka.topic=orders.v{step}\n# {marker}\n"),
        ),
        // Add / delete a consumer file.
        3 => toggle(
            "services/notify/audit_worker.py",
            format!(
                "from confluent_kafka import Consumer\n\n\ndef audit():\n    consumer = Consumer({{}})\n    consumer.subscribe([\"orders.created\"])\n# {marker}\n"
            ),
        ),
        // Add / delete a TypeScript importer of `Money`.
        4 => toggle(
            "services/web/src/refund.controller.ts",
            format!(
                "import {{ Money }} from './money';\n\nexport class RefundController {{\n  refund(): Money {{\n    return new Money();\n  }}\n}}\n// {marker}\n"
            ),
        ),
        // Evolve a proto contract.
        _ => write(
            "proto/users/v1/admin.proto",
            format!(
                "syntax = \"proto3\";\n\npackage users.v1;\n\nservice AdminService {{\n  rpc Ban (BanRequest) returns (BanResponse);\n  rpc GetStatus (StatusRequest) returns (StatusResponse);\n  rpc Audit{step} (StatusRequest) returns (StatusResponse);\n}}\n\nmessage BanRequest {{}}\nmessage BanResponse {{}}\nmessage StatusRequest {{}}\nmessage StatusResponse {{}}\n// {marker}\n"
            ),
        ),
    }
}

// ── I1: same input ⇒ same snapshot ─────────────────────────────────────────

#[test]
#[ignore = "P0 steps 1.3/1.4: the 15 ms wall-clock parse timeout and HashMap-order RPC resolution make builds load- and run-dependent"]
fn same_fingerprint_across_pool_sizes_and_runs() {
    for fixture in fixtures() {
        let (_tmp, base) = workspace_copy(&fixture);
        let config = config(&["."]);
        let roots = resolved_roots(&config, &base);
        let files = WorkspaceIndexer::crawl_all(&config, &roots);
        let reference = build_on_pool(&config, &roots, &files, 1).fingerprint();
        for run in 0..10 {
            let got = build_on_pool(&config, &roots, &files, 8).fingerprint();
            assert_eq!(
                got,
                reference,
                "{}: build #{run} on 8 threads diverged from the 1-thread build",
                fixture.display()
            );
        }
    }
}

#[test]
#[ignore = "P0 step 1.4: import and RPC resolution keep the first candidate in insertion order"]
fn shuffled_file_order_same_fingerprint() {
    for fixture in fixtures() {
        let (_tmp, base) = workspace_copy(&fixture);
        let config = config(&["."]);
        let roots = resolved_roots(&config, &base);
        let files = WorkspaceIndexer::crawl_all(&config, &roots);
        let reference = build_on_pool(&config, &roots, &files, 1).fingerprint();
        let mut rng = Lcg(0x5eed_0001);
        for round in 0..5 {
            let mut shuffled = files.clone();
            rng.shuffle(&mut shuffled);
            let got = build_on_pool(&config, &roots, &shuffled, 1).fingerprint();
            assert_eq!(
                got,
                reference,
                "{}: shuffled file order #{round} changed the snapshot",
                fixture.display()
            );
        }
    }
}

#[test]
#[ignore = "P0 step 1.4: a client naming only `AdminService` is linked to whichever homonym a HashMap iterates first"]
fn homonym_resolution_is_stable() {
    let (_tmp, base) = workspace_copy(&determinism_fixture());
    let config = config(&["."]);
    let roots = resolved_roots(&config, &base);
    let reference = full_fingerprint(&config, &roots);
    for run in 0..16 {
        assert_eq!(
            full_fingerprint(&config, &roots).graph,
            reference.graph,
            "build #{run}: the ambiguous AdminService client was linked differently"
        );
    }
}

// ── I3: reconcile is idempotent ────────────────────────────────────────────

#[test]
fn reconcile_is_idempotent() {
    for fixture in fixtures() {
        let (_tmp, base) = workspace_copy(&fixture);
        let config = config(&["."]);
        let roots = resolved_roots(&config, &base);
        let mut graph =
            WorkspaceIndexer::build_snapshot(&config, &roots, None, None).contract_graph;
        let once = graph.fingerprint();
        graph.reconcile_edges();
        assert_eq!(
            graph.fingerprint(),
            once,
            "{}: reconciling twice changed the graph",
            fixture.display()
        );
    }
}

// ── I4: one file, one set of facts ─────────────────────────────────────────

#[test]
#[ignore = "P0 step 1.2: a file under two overlapping roots is indexed once per root"]
fn overlapping_roots_index_each_file_once() {
    let (_tmp, base) = workspace_copy(&determinism_fixture());
    let config = config(&[".", "./services/*"]);
    let roots = resolved_roots(&config, &base);
    let snapshot = WorkspaceIndexer::build_snapshot(&config, &roots, None, None);

    let mut seen: HashMap<(PathBuf, usize, String, String), usize> = HashMap::new();
    for node in snapshot.contract_graph.all_nodes() {
        let key = (
            node.file_path.to_path_buf(),
            node.line_start,
            format!("{:?}", node.kind),
            node.name.to_string(),
        );
        *seen.entry(key).or_insert(0) += 1;
    }
    let mut duplicated: Vec<String> = seen
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|((path, line, kind, name), count)| {
            format!("{}:{line} {kind} {name} x{count}", path.display())
        })
        .collect();
    duplicated.sort();
    assert!(
        duplicated.is_empty(),
        "{} node(s) indexed more than once, e.g. {:?}",
        duplicated.len(),
        &duplicated[..duplicated.len().min(5)]
    );
}

// ── I2: incremental == full ────────────────────────────────────────────────

#[test]
#[ignore = "P0 steps 1.4/1.5: resolution follows HashMap order, and edges into re-indexed files or cross-file placeholder values are not rebuilt on reload"]
fn incremental_equals_full() {
    let (_tmp, base) = workspace_copy(&determinism_fixture());
    let state = indexed_state(config(&["."]), resolved_roots(&config(&["."]), &base));
    let check_config = config(&["."]);
    let check_roots = resolved_roots(&check_config, &base);

    let mut rng = Lcg(0x5eed_0002);
    for step in 0..30 {
        let op = rng.below(6);
        apply_edit(&base, op, step);
        WorkspaceIndexer::reload(&state);
        let incremental = state.snapshot().fingerprint();
        let full = full_fingerprint(&check_config, &check_roots);
        assert_eq!(
            incremental, full,
            "step {step} (edit #{op}): incremental reload diverged from a full rebuild"
        );
    }
}

#[test]
#[ignore = "P0 step 1.5: overlapping reloads can install a snapshot computed from a stale base"]
fn concurrent_reloads_converge_to_full_build() {
    let (_tmp, base) = workspace_copy(&determinism_fixture());
    let state = indexed_state(config(&["."]), resolved_roots(&config(&["."]), &base));
    let stop = Arc::new(AtomicBool::new(false));

    let reloaders: Vec<_> = (0..4)
        .map(|_| {
            let state = state.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    WorkspaceIndexer::reload(&state);
                }
            })
        })
        .collect();

    let mut rng = Lcg(0x5eed_0003);
    for step in 0..40 {
        apply_edit(&base, rng.below(6), step);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    stop.store(true, Ordering::SeqCst);
    for reloader in reloaders {
        reloader.join().expect("reloader thread");
    }

    // Quiescent now: one last reload must land exactly on a full rebuild.
    WorkspaceIndexer::reload(&state);
    let check_config = config(&["."]);
    let check_roots = resolved_roots(&check_config, &base);
    assert_eq!(
        state.snapshot().fingerprint(),
        full_fingerprint(&check_config, &check_roots),
        "concurrent reloads lost or duplicated an update"
    );
}
