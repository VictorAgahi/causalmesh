#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Plan 4 step 4.7 exit criteria: `mesh-mcp doctor [--fix] [--json]`'s
//! repairable-health checks (section 10) — socket permissions, daemon version
//! drift, a corrupt or pre-4.4 legacy cache, an orphaned per-version
//! workspace cache directory — with an isolated `HOME` and socket directory,
//! run out-of-process (like `index_cache_quota.rs`'s cache tests) so the real
//! user's `~/.cache/mesh-mcp` is never touched and parallel tests never race
//! on the same `HOME` env var.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn mesh_mcp_bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("test exe");
    path.pop(); // deps/
    path.pop(); // release/ or debug/
    path.join(if cfg!(windows) {
        "mesh-mcp.exe"
    } else {
        "mesh-mcp"
    })
}

fn write_config(dir: &Path) -> PathBuf {
    let config = dir.join("mesh-mcp.toml");
    std::fs::write(
        &config,
        "[workspace]\nname = \"doctor-test\"\nversion = \"0\"\nroots = [\".\"]\n",
    )
    .expect("write config");
    config
}

/// Runs `doctor [--fix] --json` against `home` (an isolated `$HOME`/`$USERPROFILE`)
/// and `config`, and returns the parsed section-10 checks.
fn run_doctor(home: &Path, config: &Path, fix: bool) -> Vec<Value> {
    let mut cmd = Command::new(mesh_mcp_bin());
    cmd.arg("doctor")
        .arg("--json")
        .arg("--config")
        .arg(config)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("MESH_SOCKET_PATH");
    if fix {
        cmd.arg("--fix");
    }
    let out = cmd.output().expect("run doctor");
    assert!(
        out.status.success(),
        "doctor exited non-zero: {}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout_text = String::from_utf8_lossy(&out.stdout).into_owned();
    let result: Result<Vec<Value>, _> = serde_json::from_slice(&out.stdout);
    let msg = format!("doctor --json produced invalid JSON, stdout:\n{stdout_text}");
    result.expect(&msg)
}

fn find<'a>(checks: &'a [Value], name: &str) -> Option<&'a Value> {
    checks.iter().find(|c| c["name"] == name)
}

#[test]
fn legacy_global_cache_is_reported_then_removed_by_fix() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());

    let legacy = home.path().join(".cache").join("mesh-mcp");
    std::fs::create_dir_all(&legacy).expect("mkdir");
    std::fs::write(legacy.join("index-cache.db"), b"").expect("legacy cache file");

    let checks = run_doctor(home.path(), &config, false);
    let check = find(&checks, "Legacy cache").expect("legacy cache check present");
    assert_eq!(check["status"], "warn");
    assert!(
        check.get("fixed").is_none(),
        "no fix requested yet: {check:?}"
    );

    let checks = run_doctor(home.path(), &config, true);
    let check = find(&checks, "Legacy cache").expect("legacy cache check present");
    assert_eq!(check["fixed"], true, "{check:?}");
    assert!(!legacy.join("index-cache.db").exists());

    let checks = run_doctor(home.path(), &config, false);
    assert!(
        find(&checks, "Legacy cache").is_none(),
        "removed cache must not be reported again: {checks:?}"
    );
}

#[test]
fn corrupt_workspace_cache_is_reported_then_removed_by_fix() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());
    let canonical_project = dunce::canonicalize(project.path()).expect("canon");

    let workspace_id = mesh_core::workspace_id(&canonical_project);
    let db_path = home
        .path()
        .join(".cache")
        .join("mesh-mcp")
        .join("workspaces")
        .join(&workspace_id)
        .join("index-cache.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&db_path, b"not a sqlite database").expect("corrupt db");

    let checks = run_doctor(home.path(), &config, false);
    let check = find(&checks, "Index cache").expect("index cache check present");
    assert_eq!(check["status"], "error", "{check:?}");

    let checks = run_doctor(home.path(), &config, true);
    let check = find(&checks, "Index cache").expect("index cache check present");
    assert_eq!(check["fixed"], true, "{check:?}");
    assert!(!db_path.exists());
}

#[test]
fn orphaned_workspace_cache_directory_is_reported_then_removed_by_fix() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());
    let canonical_project = dunce::canonicalize(project.path()).expect("canon");

    // A directory under `workspaces/` whose marker names this exact project, but
    // whose id is not the one `mesh_core::workspace_id` would compute today —
    // exactly what an old mesh-mcp version's leftover cache looks like.
    let fake_old_id = "deadbeefdeadbeef";
    let orphan_dir = home
        .path()
        .join(".cache")
        .join("mesh-mcp")
        .join("workspaces")
        .join(fake_old_id);
    std::fs::create_dir_all(&orphan_dir).expect("mkdir");
    std::fs::write(
        orphan_dir.join("workspace_path"),
        canonical_project.to_string_lossy().as_bytes(),
    )
    .expect("marker");
    std::fs::write(orphan_dir.join("index-cache.db"), b"").expect("db file");

    let checks = run_doctor(home.path(), &config, false);
    let check =
        find(&checks, "Orphaned workspace cache").expect("orphaned workspace cache reported");
    assert_eq!(check["status"], "warn");
    assert!(check["message"].as_str().unwrap().contains(fake_old_id));

    let checks = run_doctor(home.path(), &config, true);
    let check = find(&checks, "Orphaned workspace cache").expect("still reported once");
    assert_eq!(check["fixed"], true, "{check:?}");
    assert!(!orphan_dir.exists());
}

/// An unrelated project's cache under `workspaces/` (a different marked path)
/// must never be touched, `--fix` included — only leftovers of *this* project
/// are ever in scope.
#[test]
fn a_different_projects_workspace_cache_is_never_touched() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let other_project = tempfile::tempdir().expect("other project");
    let config = write_config(project.path());
    let canonical_other = dunce::canonicalize(other_project.path()).expect("canon");

    let other_dir = home
        .path()
        .join(".cache")
        .join("mesh-mcp")
        .join("workspaces")
        .join("some-other-id");
    std::fs::create_dir_all(&other_dir).expect("mkdir");
    std::fs::write(
        other_dir.join("workspace_path"),
        canonical_other.to_string_lossy().as_bytes(),
    )
    .expect("marker");

    let checks = run_doctor(home.path(), &config, true);
    assert!(
        find(&checks, "Orphaned workspace cache").is_none(),
        "another project's cache must never be reported or touched: {checks:?}"
    );
    assert!(
        other_dir.exists(),
        "another project's cache dir must survive --fix"
    );
}

#[test]
fn daemon_version_mismatch_is_reported_and_fix_stops_the_live_daemon() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());
    let canonical_project = dunce::canonicalize(project.path()).expect("canon");
    let workspace_id = mesh_core::workspace_id(&canonical_project);

    // A real, harmless, long-lived process standing in for an old meshd: proves
    // `--fix` actually terminates the recorded PID, not just marks it fixed.
    let mut child = Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
        .args(if cfg!(windows) {
            vec!["/C", "timeout /T 30"]
        } else {
            vec!["30"]
        })
        .spawn()
        .expect("spawn stand-in process");

    let meta = mesh_core::socket::DaemonMeta {
        pid: child.id(),
        version: "0.0.1-old".to_string(),
    };
    // `daemon_meta_path` reads `$HOME` at call time, which in this *test harness*
    // process is the real machine's home, not `home` (that override only takes
    // effect inside the doctor subprocess) — so the path is built by hand here,
    // matching `daemon_meta_path`'s own layout, rooted under the isolated `home`.
    let meta_path = home
        .path()
        .join(".cache")
        .join("mesh-mcp")
        .join("meshd")
        .join(format!("{workspace_id}.meta"));
    std::fs::create_dir_all(meta_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).expect("write meta");

    let checks = run_doctor(home.path(), &config, false);
    let check = find(&checks, "Daemon version").expect("daemon version check present");
    assert_eq!(check["status"], "warn", "{check:?}");
    assert!(check["message"].as_str().unwrap().contains("0.0.1-old"));
    assert!(check.get("fixed").is_none());

    let checks = run_doctor(home.path(), &config, true);
    let check = find(&checks, "Daemon version").expect("daemon version check present");
    assert_eq!(check["fixed"], true, "{check:?}");

    // The stand-in process must actually be gone (SIGTERM/TerminateProcess), not
    // just reported as fixed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stand-in process was not terminated"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A recorded PID that has already exited (a crashed daemon, or one that
/// already stopped) has nothing left to terminate: `--fix` never sends it a
/// signal (never risking landing on an unrelated process a later, unrelated
/// program now happens to occupy that same PID number), and instead cleans
/// up the stale record itself — that alone is a full, honestly-reported fix.
#[test]
fn daemon_version_mismatch_with_a_dead_pid_is_fixed_by_removing_the_stale_record_only() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());
    let canonical_project = dunce::canonicalize(project.path()).expect("canon");
    let workspace_id = mesh_core::workspace_id(&canonical_project);

    let mut child = Command::new(if cfg!(windows) { "cmd" } else { "true" })
        .args(if cfg!(windows) {
            vec!["/C", "exit 0"]
        } else {
            vec![]
        })
        .spawn()
        .expect("spawn short-lived process");
    let dead_pid = child.id();
    child.wait().expect("wait for exit");
    // Give the OS a moment to fully reap the process, in case a signal-0 probe
    // could otherwise race a not-yet-reclaimed PID table entry.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let meta = mesh_core::socket::DaemonMeta {
        pid: dead_pid,
        version: "0.0.1-old".to_string(),
    };
    let meta_path = home
        .path()
        .join(".cache")
        .join("mesh-mcp")
        .join("meshd")
        .join(format!("{workspace_id}.meta"));
    std::fs::create_dir_all(meta_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).expect("write meta");

    let checks = run_doctor(home.path(), &config, true);
    let check = find(&checks, "Daemon version").expect("daemon version check present");
    assert_eq!(check["fixed"], true, "{check:?}");
    assert!(!meta_path.exists(), "the stale record must be removed");

    // A second run has nothing left to report: the record is gone, not just
    // marked fixed once and left behind to be reported forever after.
    let checks = run_doctor(home.path(), &config, false);
    assert!(
        find(&checks, "Daemon version").is_none(),
        "must not be reported again once the stale record is gone: {checks:?}"
    );
}

#[test]
fn daemon_version_match_is_ok_and_offers_no_fix() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());
    let canonical_project = dunce::canonicalize(project.path()).expect("canon");
    let workspace_id = mesh_core::workspace_id(&canonical_project);

    let meta = mesh_core::socket::DaemonMeta {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let meta_path = home
        .path()
        .join(".cache")
        .join("mesh-mcp")
        .join("meshd")
        .join(format!("{workspace_id}.meta"));
    std::fs::create_dir_all(meta_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).expect("write meta");

    let checks = run_doctor(home.path(), &config, true);
    let check = find(&checks, "Daemon version").expect("daemon version check present");
    assert_eq!(check["status"], "ok", "{check:?}");
    assert!(
        check.get("fixed").is_none(),
        "a match has nothing to fix: {check:?}"
    );
}

#[test]
#[cfg(unix)]
fn socket_permissions_are_reported_then_repaired_by_fix() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());
    let canonical_project = dunce::canonicalize(project.path()).expect("canon");
    let workspace_id = mesh_core::workspace_id(&canonical_project);

    let sock_dir = home.path().join(".cache").join("mesh");
    std::fs::create_dir_all(&sock_dir).expect("mkdir");
    let sock_path = sock_dir.join(format!("meshd-{workspace_id}.sock"));
    // A real, live listener: doctor now tries connecting before checking
    // permissions (an orphaned socket, step below, takes a different path),
    // so this must actually be one for the permission check to be reached.
    let listener = std::os::unix::net::UnixListener::bind(&sock_path).expect("bind fake daemon");
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o666)).unwrap();
    std::fs::set_permissions(&sock_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let checks = run_doctor(home.path(), &config, false);
    let check = find(&checks, "Socket permissions").expect("socket permissions check present");
    assert_eq!(check["status"], "warn", "{check:?}");

    let checks = run_doctor(home.path(), &config, true);
    let check = find(&checks, "Socket permissions").expect("socket permissions check present");
    assert_eq!(check["fixed"], true, "{check:?}");

    let sock_mode = std::fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(sock_mode, 0o600);
    let dir_mode = std::fs::metadata(&sock_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);

    drop(listener);
}

/// The other half of the socket check: a socket file with nothing listening
/// behind it (the process that bound it crashed or was killed without
/// cleaning up) is reported and, on `--fix`, removed — mirroring `meshd`'s own
/// `cleanup_stale_socket` startup check, but from `doctor` on demand.
#[test]
fn orphaned_socket_is_reported_and_removed_by_fix() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());
    let canonical_project = dunce::canonicalize(project.path()).expect("canon");
    let workspace_id = mesh_core::workspace_id(&canonical_project);

    let sock_dir = home.path().join(".cache").join("mesh");
    std::fs::create_dir_all(&sock_dir).expect("mkdir");
    let sock_path = sock_dir.join(format!("meshd-{workspace_id}.sock"));
    // Bind then drop: the socket file survives (Unix does not clean it up on
    // its own), but nothing is listening any more — a stale leftover.
    drop(std::os::unix::net::UnixListener::bind(&sock_path).expect("bind then orphan"));
    assert!(
        sock_path.exists(),
        "the socket file must outlive its listener"
    );

    let checks = run_doctor(home.path(), &config, false);
    let check = find(&checks, "Orphaned socket").expect("orphaned socket check present");
    assert_eq!(check["status"], "warn", "{check:?}");
    assert!(check.get("fixed").is_none());

    let checks = run_doctor(home.path(), &config, true);
    let check = find(&checks, "Orphaned socket").expect("orphaned socket check present");
    assert_eq!(check["fixed"], true, "{check:?}");
    assert!(
        !sock_path.exists(),
        "an orphaned socket must be removed by --fix"
    );
}

#[test]
fn audit_db_corruption_is_reported_but_never_fixed() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());

    let audit_path = home.path().join(".cache").join("mesh-mcp").join("audit.db");
    std::fs::create_dir_all(audit_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&audit_path, b"not a sqlite database").expect("corrupt audit db");

    let checks = run_doctor(home.path(), &config, true);
    let check = find(&checks, "Audit trail").expect("audit trail check present");
    assert_eq!(check["status"], "error", "{check:?}");
    assert!(
        check.get("fixed").is_none(),
        "the audit trail must never offer or report a fix: {check:?}"
    );
    assert!(
        audit_path.exists(),
        "a corrupt audit db must never be deleted"
    );
}

/// The plan's own combined exit criterion, literally: an orphaned socket, a
/// corrupt cache, and a daemon of another version, all at once — `doctor
/// --fix` handles every one of them, and a second `doctor` is clean.
#[test]
fn orphaned_socket_corrupt_cache_and_stale_daemon_together_are_all_fixed_at_once() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let config = write_config(project.path());
    let canonical_project = dunce::canonicalize(project.path()).expect("canon");
    let workspace_id = mesh_core::workspace_id(&canonical_project);

    // Orphaned socket.
    let sock_dir = home.path().join(".cache").join("mesh");
    std::fs::create_dir_all(&sock_dir).expect("mkdir");
    let sock_path = sock_dir.join(format!("meshd-{workspace_id}.sock"));
    drop(std::os::unix::net::UnixListener::bind(&sock_path).expect("bind then orphan"));

    // Corrupt workspace cache.
    let db_path = home
        .path()
        .join(".cache")
        .join("mesh-mcp")
        .join("workspaces")
        .join(&workspace_id)
        .join("index-cache.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&db_path, b"not a sqlite database").expect("corrupt db");

    // Daemon of another version (a real, alive stand-in process for the PID).
    let mut child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn stand-in");
    let meta = mesh_core::socket::DaemonMeta {
        pid: child.id(),
        version: "0.0.1-old".to_string(),
    };
    let meta_path = home
        .path()
        .join(".cache")
        .join("mesh-mcp")
        .join("meshd")
        .join(format!("{workspace_id}.meta"));
    std::fs::create_dir_all(meta_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).expect("write meta");

    // One `--fix` run treats all three.
    let checks = run_doctor(home.path(), &config, true);
    for name in ["Orphaned socket", "Index cache", "Daemon version"] {
        let check = find(&checks, name).expect(name);
        assert_eq!(check["fixed"], true, "{name}: {check:?}");
    }

    assert!(!sock_path.exists());
    assert!(!db_path.exists());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while child.try_wait().ok().flatten().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "stand-in process was not terminated"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // A second doctor run is clean: nothing left to report or fix.
    let checks = run_doctor(home.path(), &config, false);
    for name in ["Orphaned socket", "Index cache", "Daemon version"] {
        assert!(
            find(&checks, name).is_none(),
            "{name} must not be reported after everything was fixed: {checks:?}"
        );
    }
}
