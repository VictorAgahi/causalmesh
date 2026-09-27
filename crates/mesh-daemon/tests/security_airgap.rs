//! Plan 4 step 4.10 — kernel network sandbox of `meshd` (Linux only).
//!
//! Confining a process is irreversible, and cargo runs every test of a binary
//! as threads of one process: the confinement itself therefore runs in a child
//! process (this test binary re-executed on an `#[ignore]`d test, selected by
//! an environment variable), never in the shared test process.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../src/sandbox.rs"]
#[allow(dead_code)]
mod sandbox;

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "MESH_TEST_AIRGAP_CHILD_DIR";

fn assert_eperm(what: &str, res: std::io::Result<()>) {
    let err = res.expect_err(&format!("{what} must be refused once confined"));
    assert_eq!(
        err.raw_os_error(),
        Some(libc::EPERM),
        "{what}: expected EPERM from the seccomp filter, got {err:?}"
    );
}

/// Every network probe must fail with `EPERM`: the filter rejects the
/// `socket(2)` call itself, so the (unroutable, unused) targets are never tried.
fn assert_network_denied(thread: &str) {
    let t = Duration::from_millis(200);
    assert_eperm(
        &format!("[{thread}] TCP/IPv4 connect"),
        TcpStream::connect_timeout(&"127.0.0.1:9".parse().unwrap(), t).map(drop),
    );
    assert_eperm(
        &format!("[{thread}] TCP/IPv6 connect"),
        TcpStream::connect_timeout(&"[::1]:9".parse().unwrap(), t).map(drop),
    );
    assert_eperm(
        &format!("[{thread}] UDP/IPv4 bind"),
        UdpSocket::bind("127.0.0.1:0").map(drop),
    );
    // io_uring could create sockets without `socket(2)` (IORING_OP_SOCKET).
    let rc = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            1u32,
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert!(
        rc == -1 && errno == Some(libc::EPERM),
        "[{thread}] io_uring_setup must fail with EPERM, got rc={rc} errno={errno:?}"
    );
}

/// `AF_UNIX` — `meshd`'s own transport — keeps working end to end.
fn assert_unix_socket_works(dir: &Path, name: &str) {
    let path = dir.join(name);
    let listener = UnixListener::bind(&path).expect("AF_UNIX bind must stay allowed");
    let mut client = UnixStream::connect(&path).expect("AF_UNIX connect must stay allowed");
    let (mut server, _) = listener.accept().expect("AF_UNIX accept");
    client.write_all(b"ping").expect("AF_UNIX write");
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).expect("AF_UNIX read");
    assert_eq!(&buf, b"ping");
}

/// Seccomp mode of every thread of `pid` (`/proc/<pid>/task/*/status`).
fn thread_seccomp_modes(pid: u32) -> Vec<String> {
    let mut modes = Vec::new();
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return modes;
    };
    for task in tasks.flatten() {
        let Ok(status) = std::fs::read_to_string(task.path().join("status")) else {
            continue;
        };
        if let Some(mode) = status.lines().find_map(|l| l.strip_prefix("Seccomp:")) {
            modes.push(mode.trim().to_string());
        }
    }
    modes
}

/// Child half of `threads_before_and_after_confinement_cannot_open_inet_sockets`.
#[test]
#[ignore = "spawned as a child process by threads_before_and_after_confinement_cannot_open_inet_sockets"]
fn child_confine_then_probe() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);

    // Sanity: before confinement the probes do succeed (so EPERM below is the
    // filter's doing, not the runner's), and AF_UNIX works.
    UdpSocket::bind("127.0.0.1:0").expect("UDP must work before confinement");
    assert_unix_socket_works(&dir, "pre.sock");

    // A thread created BEFORE the filter, parked until it is installed.
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let before_dir = dir.clone();
    let before = std::thread::spawn(move || {
        go_rx.recv().expect("go signal");
        assert_network_denied("thread created before confinement");
        assert_unix_socket_works(&before_dir, "before.sock");
    });

    sandbox::confine_network().expect("seccomp filter must install on the CI runner");
    go_tx.send(()).expect("wake pre-existing thread");
    before.join().expect("pre-existing thread assertions");

    // A thread created AFTER the filter (inherits it).
    let after_dir = dir.clone();
    std::thread::spawn(move || {
        assert_network_denied("thread created after confinement");
        assert_unix_socket_works(&after_dir, "after.sock");
    })
    .join()
    .expect("post-confinement thread assertions");

    assert_network_denied("confining thread");

    // Documented choice: only AF_INET/AF_INET6 are denied; other local
    // families (here AF_NETLINK) are left alone.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    assert!(
        fd >= 0,
        "AF_NETLINK stays allowed: {:?}",
        std::io::Error::last_os_error()
    );
    unsafe { libc::close(fd) };

    let modes = thread_seccomp_modes(std::process::id());
    assert!(
        !modes.is_empty() && modes.iter().all(|m| m == "2"),
        "every thread must be in seccomp filter mode (TSYNC), got {modes:?}"
    );

    std::fs::write(dir.join("child-ok"), b"ok").expect("marker");
}

#[test]
fn threads_before_and_after_confinement_cannot_open_inet_sockets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = std::env::current_exe().expect("test exe");
    let out = Command::new(exe)
        .args([
            "child_confine_then_probe",
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD_ENV, dir.path())
        .output()
        .expect("spawn child");
    assert!(
        out.status.success(),
        "confined child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.path().join("child-ok").exists(),
        "child test did not run to completion:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // The parent process was never confined.
    UdpSocket::bind("127.0.0.1:0").expect("test process itself must stay unconfined");
}

/// End to end: the real `meshd` binary confines itself right after binding its
/// socket — every one of its threads ends up in seccomp filter mode — and
/// still accepts clients over that socket. `MESH_DAEMON_SANDBOX=required`
/// makes a failed install exit the daemon, which this test would catch.
#[test]
fn meshd_confines_every_thread_after_bind_and_still_serves_uds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&ws).expect("ws");
    let config = ws.join("mesh-mcp.toml");
    std::fs::write(
        &config,
        "[workspace]\nname = \"airgap\"\nversion = \"1.0.0\"\nroots = [\".\"]\n",
    )
    .expect("config");
    std::fs::write(ws.join("a.go"), "package a\n\nfunc A() {}\n").expect("source");
    let sock = dir.path().join("s.sock");
    let log_path = dir.path().join("meshd.log");
    let log = std::fs::File::create(&log_path).expect("log");

    let mut child = Command::new(env!("CARGO_BIN_EXE_meshd"))
        .arg("--config")
        .arg(&config)
        .arg("--socket")
        .arg(&sock)
        .args(["--idle-timeout-minutes", "0", "--startup-grace-secs", "0"])
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("MESH_DAEMON_SANDBOX", "required")
        .current_dir(&ws)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn meshd");
    let pid = child.id();

    let deadline = Instant::now() + Duration::from_secs(30);
    let read_log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    let (confined, modes) = loop {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("meshd exited early ({status}):\n{}", read_log());
        }
        let modes = thread_seccomp_modes(pid);
        if sock.exists() && !modes.is_empty() && modes.iter().all(|m| m == "2") {
            break (true, modes);
        }
        if Instant::now() > deadline {
            break (false, modes);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let connect = UnixStream::connect(&sock);
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        confined,
        "every meshd thread must reach seccomp mode 2 after bind, got {modes:?}\n{}",
        read_log()
    );
    match connect {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => panic!("socket vanished: {e}"),
        Err(e) => panic!("confined meshd must still accept AF_UNIX clients: {e}"),
    }
}
