//! Kernel-level network sandbox for `meshd` (plan 4 step 4.10, Linux only).
//!
//! `meshd` never needs the network: it indexes local files, answers over a Unix
//! domain socket, and only ever spawns local `git`/`ps`/`pgrep` children. Once
//! its socket is bound, [`apply_after_bind`] installs a seccomp-bpf filter that
//! makes the kernel refuse, with `EPERM`:
//!
//! - `socket(AF_INET, …)` and `socket(AF_INET6, …)` — every TCP/UDP/raw IP
//!   socket, hence every outbound connection or DNS lookup;
//! - `io_uring_setup` — `IORING_OP_SOCKET` (Linux 5.19+) creates sockets
//!   without going through the `socket` syscall, so a ring would bypass the
//!   first rule. Nothing in `meshd` uses io_uring (Tokio uses epoll).
//!
//! Every other syscall, and every other address family, is allowed: `AF_UNIX`
//! is the daemon's own transport, and families such as `AF_NETLINK` are local
//! kernel interfaces that glibc may use internally (e.g. `getifaddrs`), not a
//! route off the machine. The filter is a deny-list on purpose: an allow-list
//! of every syscall Tokio, rayon, `notify`, SQLite and tree-sitter might make
//! would turn any libc/kernel upgrade into a potential crash of the daemon.
//!
//! The filter is installed with `SECCOMP_FILTER_FLAG_TSYNC` (via
//! [`seccompiler::apply_filter_all_threads`], which also sets
//! `PR_SET_NO_NEW_PRIVS`), so it covers every thread that already exists when
//! it is applied — Tokio workers and blocking pool, rayon, the `notify`
//! watcher — not only the calling one. Threads and child processes created
//! later inherit it.
//!
//! **Failure policy.** If the kernel refuses the filter (seccomp disabled, a
//! sandbox that forbids `seccomp(2)`, gVisor without TSYNC), `meshd` logs a
//! `warn` and keeps serving by default: the daemon has no network code to begin
//! with, so the filter is defence in depth, and refusing to start would
//! silently push every IDE client into its standalone fallback — unconfined
//! too. Operators who need the guarantee set `MESH_DAEMON_SANDBOX=required`:
//! `meshd` then refuses to serve instead (on every OS: outside Linux there is
//! no sandbox, so `required` always refuses). Any other non-empty value is
//! logged and treated as `required`.
//!
//! Limits: only `meshd` is confined — the `mesh-mcp run` stdio proxy,
//! `mesh-mcp run --standalone` and `mesh-mcp graph --open` (which launches a
//! browser) are not. macOS and Windows have no equivalent here.

/// Environment variable selecting the failure policy (see the module docs).
pub const POLICY_ENV: &str = "MESH_DAEMON_SANDBOX";

/// Whether a failure to install the sandbox must stop the daemon.
///
/// Unset or empty means best effort; `required` (any case) means fail closed.
/// Any other value is a misconfiguration: it is logged and treated as
/// `required`, since an operator who set the variable at all asked for more
/// than the default (a `MESH_DAEMON_SANDBOX=1` or a typo such as `require`
/// must never silently leave a host unconfined).
fn sandbox_required() -> bool {
    policy_is_required(std::env::var_os(POLICY_ENV).as_deref())
}

fn policy_is_required(raw: Option<&std::ffi::OsStr>) -> bool {
    let Some(raw) = raw else {
        return false;
    };
    let value = raw.to_string_lossy();
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if !value.eq_ignore_ascii_case("required") {
        tracing::warn!(
            target: "meshd::sandbox",
            "Unrecognized {POLICY_ENV}={value:?} (only `required` is accepted); treating it as `required`."
        );
    }
    true
}

/// Confines the calling process (all its threads) — see the module docs.
/// Called by `meshd` right after its socket is bound. Returns `Err` only when
/// the sandbox could not be installed **and** `MESH_DAEMON_SANDBOX` asks for it
/// to be required; otherwise a failure is logged and swallowed. Never panics.
pub fn apply_after_bind() -> Result<(), String> {
    // Parsed up front so a malformed value is reported even when the filter
    // installs fine.
    let required = sandbox_required();
    match confine_network() {
        Ok(()) => {
            tracing::info!(
                target: "meshd::sandbox",
                "Network sandbox active: socket(AF_INET/AF_INET6) and io_uring_setup now fail with EPERM on every thread."
            );
            Ok(())
        }
        Err(e) if required => Err(format!(
            "network sandbox could not be installed ({e}) and {POLICY_ENV}=required"
        )),
        Err(e) => {
            #[cfg(target_os = "linux")]
            tracing::warn!(
                target: "meshd::sandbox",
                "Network sandbox NOT installed ({e}); meshd keeps running unconfined. Set {POLICY_ENV}=required to refuse to start instead."
            );
            #[cfg(not(target_os = "linux"))]
            tracing::debug!(target: "meshd::sandbox", "Network sandbox unavailable: {e}");
            Ok(())
        }
    }
}

/// Installs the network deny filter on every thread of the process. Limited to
/// the architectures seccompiler generates filters for.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
pub fn confine_network() -> Result<(), String> {
    use seccompiler::{
        apply_filter_all_threads, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp,
        SeccompCondition, SeccompFilter, SeccompRule, TargetArch,
    };
    use std::collections::BTreeMap;

    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;

    let domain_is = |family: libc::c_int| -> Result<SeccompRule, String> {
        let cond = SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            u64::from(family.unsigned_abs()),
        )
        .map_err(|e| e.to_string())?;
        SeccompRule::new(vec![cond]).map_err(|e| e.to_string())
    };
    let socket_rules = vec![domain_is(libc::AF_INET)?, domain_is(libc::AF_INET6)?];

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    rules.insert(libc::SYS_socket, socket_rules.clone());
    // An empty rule chain matches the syscall unconditionally.
    rules.insert(libc::SYS_io_uring_setup, Vec::new());
    // The x32 ABI shares `AUDIT_ARCH_X86_64` (so seccompiler's architecture
    // check lets it through) but numbers its syscalls with bit 30 set: deny
    // those spellings too, or a kernel built with `CONFIG_X86_X32` would offer
    // a bypass. (Foreign architectures such as i386 `int 0x80` fail the
    // architecture check and are killed by seccompiler's prologue.)
    #[cfg(target_arch = "x86_64")]
    {
        const X32_SYSCALL_BIT: i64 = 0x4000_0000;
        rules.insert(X32_SYSCALL_BIT | libc::SYS_socket, socket_rules);
        rules.insert(X32_SYSCALL_BIT | libc::SYS_io_uring_setup, Vec::new());
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM.unsigned_abs()),
        arch,
    )
    .map_err(|e| e.to_string())?;
    let program = BpfProgram::try_from(filter).map_err(|e| e.to_string())?;
    apply_filter_all_threads(&program).map_err(|e| e.to_string())
}

/// No kernel sandbox outside Linux (plan 4 step 4.10: macOS is out of scope).
#[cfg(not(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
)))]
pub fn confine_network() -> Result<(), String> {
    Err(format!(
        "no network sandbox on {}/{} (Linux seccomp on x86_64, aarch64, riscv64 only)",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

#[cfg(test)]
mod tests {
    use super::policy_is_required;
    use std::ffi::OsStr;

    #[test]
    fn policy_fails_closed_on_anything_but_unset_or_empty() {
        assert!(!policy_is_required(None));
        assert!(!policy_is_required(Some(OsStr::new(""))));
        assert!(!policy_is_required(Some(OsStr::new("  "))));
        assert!(policy_is_required(Some(OsStr::new("required"))));
        assert!(policy_is_required(Some(OsStr::new(" REQUIRED "))));
        // Misspellings and truthy values are not silently ignored.
        assert!(policy_is_required(Some(OsStr::new("require"))));
        assert!(policy_is_required(Some(OsStr::new("1"))));
    }
}
