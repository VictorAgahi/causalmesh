use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr, ValidatedScope};
use mesh_parsers::languages::proto::{
    diff_wire_schemas, ProtoExtractor, WireDiff, WireSchemaError,
};
use mesh_parsers::{AstGuard, MarkdownFormatter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeGrpcArgs {
    #[schemars(
        with = "String",
        description = "Name of the gRPC service (ex: 'UserService'), RPC method (ex: 'SignUp', 'AuthenticateUser'), or package. A path ending in '.proto' (inside the workspace roots) runs the wire-format check on that file directly."
    )]
    pub target: CompactStr,

    /// Git revision the `.proto` is compared against for wire-format breaking
    /// changes. See `AnalyzeGrpcTool::DESCRIPTION`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Git revision to compare the .proto against for wire-format breaking changes (ex: 'main', 'origin/main', 'v1.4.0', a commit SHA). Omit to use the merge-base of HEAD with the first existing of origin/HEAD, origin/main, main (falling back to HEAD). NOT a file path; must not start with '-' or contain ':'."
    )]
    pub base: Option<String>,

    #[serde(default)]
    // Accepted for W3C trace propagation, hidden from `tools/list`: the model
    // cannot use it, and it cost every session ~200 schema tokens.
    #[schemars(skip)]
    pub _meta: Option<RequestMeta>,
}

pub struct AnalyzeGrpcTool;

impl McpTool for AnalyzeGrpcTool {
    const NAME: &'static str = "analyze_grpc";
    const DESCRIPTION: &'static str = "Traces end-to-end gRPC RPC definitions from .proto to polyglot generated stubs and controllers, then checks the defining .proto for WIRE_FORMAT_BREAKING_CHANGE against a Git base (field number reused, incompatible field type, field deleted without `reserved`). The comparison reads the base version in memory with `git show`; it never writes to disk. DO NOT USE for message brokers or asynchronous event streams (use analyze_impact). DO NOT pass a file path or a `--option` as `base`.";
    type Args = AnalyzeGrpcArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.target.as_str())
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        run_with_git(args, state, &GitRunner::system())
    }
}

/// `run`, with the `git` lookup injected so tests can point it at an empty
/// `PATH` or an isolated environment without mutating the (shared) process env.
fn run_with_git(
    args: &AnalyzeGrpcArgs,
    state: &AppState,
    git: &GitRunner,
) -> Result<ToolOutput, ToolError> {
    let explicit_base = args.base.as_deref();
    if let Some(base) = explicit_base {
        validate_base(base).map_err(|e| (-32602, e.to_string()))?;
    }

    let snapshot = state.snapshot();
    let trace = snapshot.contract_graph.analyze_grpc(args.target.as_str());
    let mut text = MarkdownFormatter::format_grpc_trace(&trace);

    // The `.proto` whose wire format is checked: the target itself when it is a
    // `.proto` path, else the file of the resolved proto definition.
    let target = args.target.as_str().trim();
    let proto_file: Option<PathBuf> = if target.ends_with(".proto") {
        Some(PathBuf::from(target))
    } else {
        trace
            .proto_definition
            .map(|n| n.file_path.to_path_buf())
            .filter(|p| p.extension().is_some_and(|e| e == "proto"))
    };

    let Some(proto_file) = proto_file else {
        if explicit_base.is_some() {
            text.push_str(
                "\n### Wire-format check\n*Skipped: no `.proto` definition is indexed for this target. Pass the `.proto` path as `target` to check a file directly.*\n",
            );
        }
        return Ok(ToolOutput::text(text));
    };

    let resolved = ValidatedScope::resolve_with_aliases(
        &proto_file.to_string_lossy(),
        &state.allowed_roots,
        &state.config.workspace.mount_aliases,
        state.config.workspace.resolved_workspace_root.as_deref(),
    );
    let file = match resolved {
        Ok(scope) => scope.into_path_buf(),
        // A path outside the jail is refused outright (commandment 4), whether it
        // came from the caller or from the index.
        Err(e @ mesh_core::SecurityError::SandboxEscapeAttempt(_)) => {
            return Err((e.jsonrpc_code(), e.to_string()))
        }
        Err(e) => {
            if explicit_base.is_some() {
                return Err((
                    e.jsonrpc_code(),
                    format!(
                        "Wire-format check: `{}` is not readable in the working tree ({e})",
                        proto_file.display()
                    ),
                ));
            }
            text.push_str(&format!(
                "\n### Wire-format check\n*Skipped: `{}` is not present in the working tree.*\n",
                proto_file.display()
            ));
            return Ok(ToolOutput::text(text));
        }
    };

    let files_accessed = vec![file.to_string_lossy().into_owned()];
    match check_wire_format(&file, explicit_base, git) {
        Ok(report) => text.push_str(&render_report(&report)),
        // Explicit base: every failure is the tool's answer (`isError`). Implicit
        // base: the trace is still valid, so a workspace that is not a Git
        // repository (or has no `git`) keeps its trace and gets a one-line note.
        Err(e) if explicit_base.is_some() || e.is_invalid_request() => {
            return Err((e.code(), format!("Wire-format check failed: {e}")))
        }
        Err(e) => text.push_str(&format!("\n### Wire-format check\n*Skipped: {e}*\n")),
    }

    Ok(ToolOutput {
        text,
        files_accessed,
        secrets_redacted: 0,
    })
}

// ---------------------------------------------------------------------------
// Git plumbing
// ---------------------------------------------------------------------------

/// Per-command budget: every call runs on an agent's synchronous request path.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Default base chain, in order (plan 4.6b). Never `HEAD~1`.
const DEFAULT_BASE_CHAIN: [&str; 3] = ["origin/HEAD", "origin/main", "main"];

/// Longest `base` accepted: a ref name or an expression like `main~3`, never a blob.
const MAX_BASE_LEN: usize = 256;

/// Git variables that would redirect every command to another repository
/// (they are set, for instance, when the server is spawned from a Git hook).
const REPO_REDIRECT_VARS: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

#[derive(Debug, thiserror::Error)]
pub(crate) enum WireCheckError {
    #[error("`base` {0:?} is invalid: {1}")]
    InvalidBase(String, &'static str),
    #[error(
        "`git` was not found on the PATH; install Git or add it to the PATH of the MeshMCP process"
    )]
    GitNotFound,
    #[error("`{0}` is not inside a Git repository")]
    NotARepository(PathBuf),
    #[error("Git base `{0}` was not found in the repository of this file")]
    BaseNotFound(String),
    #[error("`git {0}` did not finish within {1:?}")]
    Timeout(String, Duration),
    #[error("`git {0}` failed: {1}")]
    GitFailed(String, String),
    #[error("cannot read `{0}`: {1}")]
    Io(PathBuf, String),
}

impl WireCheckError {
    /// A malformed argument is `-32602` even when the base was implicit (it never
    /// is, but the classification stays total); everything else is an
    /// environment failure, reported as a server-side error code.
    fn code(&self) -> i32 {
        if self.is_invalid_request() {
            -32602
        } else {
            -32603
        }
    }

    fn is_invalid_request(&self) -> bool {
        matches!(self, Self::InvalidBase(..))
    }
}

/// Rejects anything `git` could read as an option or a path expression:
/// a `base` is passed as its own argv entry (no shell), and additionally must
/// not start with `-` (option injection), nor contain `:` (`rev:path` would
/// point at another file), whitespace or control characters.
fn validate_base(base: &str) -> Result<(), WireCheckError> {
    let fail = |why| Err(WireCheckError::InvalidBase(base.to_string(), why));
    if base.is_empty() {
        return fail("empty");
    }
    if base.len() > MAX_BASE_LEN {
        return fail("longer than 256 bytes");
    }
    if base.starts_with('-') {
        return fail("must not start with '-'");
    }
    if base.contains(':') {
        return fail("must not contain ':'");
    }
    if base.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return fail("must not contain whitespace or control characters");
    }
    Ok(())
}

struct GitOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: String,
}

impl GitOutput {
    fn stdout_line(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// Runs `git` without a shell, one argv entry per argument, with a timeout and
/// bounded output. `path` is where `git` is looked up (`None`: the process
/// `PATH`); `env` is added to the child's environment (tests use it to isolate
/// `HOME` and the global Git config).
pub(crate) struct GitRunner {
    path: Option<OsString>,
    env: Vec<(OsString, OsString)>,
    timeout: Duration,
}

impl GitRunner {
    pub(crate) fn system() -> Self {
        Self {
            path: std::env::var_os("PATH"),
            env: Vec::new(),
            timeout: GIT_COMMAND_TIMEOUT,
        }
    }

    fn locate(&self) -> Result<PathBuf, WireCheckError> {
        let path = self.path.as_ref().ok_or(WireCheckError::GitNotFound)?;
        let names: &[&str] = if cfg!(windows) {
            &["git.exe", "git.cmd", "git"]
        } else {
            &["git"]
        };
        std::env::split_paths(path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
            .find(|candidate| candidate.is_file())
            .ok_or(WireCheckError::GitNotFound)
    }

    fn run(
        &self,
        program: &Path,
        cwd: &Path,
        args: &[&OsStr],
        stdout_limit: u64,
    ) -> Result<GitOutput, WireCheckError> {
        let display = args
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let mut cmd = Command::new(program);
        // No fsmonitor hook, no pager, no credential prompt, stable messages.
        cmd.args(["-c", "core.fsmonitor=false", "--no-pager"])
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in REPO_REDIRECT_VARS {
            cmd.env_remove(var);
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => WireCheckError::GitNotFound,
            _ => WireCheckError::GitFailed(display.clone(), e.to_string()),
        })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stdout {
                // `+ 1` so an over-limit blob is detectable; dropping the pipe
                // afterwards makes `git` exit on EPIPE instead of blocking.
                let _ = s.take(stdout_limit + 1).read_to_end(&mut buf);
            }
            buf
        });
        let err_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stderr {
                let _ = s.take(8 * 1024).read_to_end(&mut buf);
            }
            buf
        });

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(WireCheckError::Timeout(display, self.timeout));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => return Err(WireCheckError::GitFailed(display, e.to_string())),
            }
        };
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        if stdout.len() as u64 > stdout_limit {
            return Err(WireCheckError::GitFailed(
                display,
                format!("output exceeds {stdout_limit} bytes"),
            ));
        }
        Ok(GitOutput {
            success: status.success(),
            stdout,
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        })
    }
}

/// Small outputs (a SHA, a path prefix).
const SMALL_OUTPUT: u64 = 64 * 1024;

/// How the base revision was chosen, for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BaseChoice {
    /// The caller's `base`.
    Explicit(String),
    /// `git merge-base HEAD <ref>`.
    MergeBase(&'static str),
    /// No ref of the default chain exists, or the merge-base failed.
    HeadFallback,
}

#[derive(Debug)]
pub(crate) struct WireReport {
    rel_path: String,
    base: BaseChoice,
    base_sha: String,
    outcome: WireOutcome,
}

#[derive(Debug)]
enum WireOutcome {
    /// The file does not exist in the base: new file, nothing to break.
    NewFile,
    /// One side could not be parsed; no finding is claimed.
    Unparseable {
        side: &'static str,
        reason: WireSchemaError,
    },
    Compared(WireDiff),
}

/// Compares the working-tree `file` (already jailed by `ValidatedScope`) with
/// its version at the base revision, read in memory with `git show`.
pub(crate) fn check_wire_format(
    file: &Path,
    base: Option<&str>,
    git: &GitRunner,
) -> Result<WireReport, WireCheckError> {
    if let Some(b) = base {
        validate_base(b)?;
    }
    let program = git.locate()?;
    let dir = file
        .parent()
        .ok_or_else(|| WireCheckError::NotARepository(file.to_path_buf()))?;
    let file_name = file
        .file_name()
        .ok_or_else(|| WireCheckError::NotARepository(file.to_path_buf()))?;
    let run = |args: &[&OsStr], limit: u64| git.run(&program, dir, args, limit);
    let os = |s: &'static str| OsStr::new(s);

    // `git` runs in the file's own directory: each configured root may be its
    // own repository (or none). `--show-prefix` gives the directory relative to
    // the repository top level, so no canonical-path comparison is needed.
    let prefix = run(&[os("rev-parse"), os("--show-prefix")], SMALL_OUTPUT)?;
    if !prefix.success {
        return Err(WireCheckError::NotARepository(dir.to_path_buf()));
    }
    let rel_path = format!(
        "{}{}",
        String::from_utf8_lossy(&prefix.stdout).trim_end_matches(['\n', '\r']),
        file_name.to_string_lossy()
    );

    let verify = |rev: &str| -> Result<Option<String>, WireCheckError> {
        let spec = format!("{rev}^{{commit}}");
        let out = run(
            &[
                os("rev-parse"),
                os("--verify"),
                os("--quiet"),
                os("--end-of-options"),
                OsStr::new(&spec),
            ],
            SMALL_OUTPUT,
        )?;
        Ok(out.success.then(|| out.stdout_line()))
    };

    let (choice, base_sha) = match base {
        Some(b) => {
            let sha = verify(b)?.ok_or_else(|| WireCheckError::BaseNotFound(b.to_string()))?;
            (BaseChoice::Explicit(b.to_string()), sha)
        }
        None => {
            let mut chosen = None;
            for candidate in DEFAULT_BASE_CHAIN {
                if verify(candidate)?.is_none() {
                    continue;
                }
                // The first existing ref decides; a failed merge-base (unrelated
                // histories) falls back to HEAD rather than trying the next ref.
                let mb = run(
                    &[os("merge-base"), os("HEAD"), OsStr::new(candidate)],
                    SMALL_OUTPUT,
                )?;
                if mb.success {
                    chosen = Some((BaseChoice::MergeBase(candidate), mb.stdout_line()));
                }
                break;
            }
            match chosen {
                Some(c) => c,
                None => {
                    let head = verify("HEAD")?.ok_or_else(|| {
                        WireCheckError::BaseNotFound("HEAD (the repository has no commit)".into())
                    })?;
                    (BaseChoice::HeadFallback, head)
                }
            }
        }
    };

    let object = format!("{base_sha}:{rel_path}");
    let exists = run(
        &[os("cat-file"), os("-e"), OsStr::new(&object)],
        SMALL_OUTPUT,
    )?;
    if !exists.success {
        return Ok(WireReport {
            rel_path,
            base: choice,
            base_sha,
            outcome: WireOutcome::NewFile,
        });
    }
    let shown = run(
        &[os("show"), os("--no-textconv"), OsStr::new(&object)],
        AstGuard::MAX_SCHEMA_FILE_SIZE_BYTES,
    )?;
    if !shown.success {
        return Err(WireCheckError::GitFailed(
            format!("show {object}"),
            shown.stderr,
        ));
    }
    let before = String::from_utf8_lossy(&shown.stdout);
    let after = read_bounded(file)?;

    let outcome = match (
        ProtoExtractor::wire_schema(&before),
        ProtoExtractor::wire_schema(&after),
    ) {
        (Err(reason), _) => WireOutcome::Unparseable {
            side: "base",
            reason,
        },
        (_, Err(reason)) => WireOutcome::Unparseable {
            side: "working tree",
            reason,
        },
        (Ok(old), Ok(new)) => WireOutcome::Compared(diff_wire_schemas(&old, &new)),
    };
    Ok(WireReport {
        rel_path,
        base: choice,
        base_sha,
        outcome,
    })
}

/// Reads the working-tree file, refusing it before the read when it exceeds the
/// schema budget (the parser guard would reject it anyway).
fn read_bounded(file: &Path) -> Result<String, WireCheckError> {
    let io = |e: std::io::Error| WireCheckError::Io(file.to_path_buf(), e.to_string());
    let meta = std::fs::metadata(file).map_err(io)?;
    if meta.len() > AstGuard::MAX_SCHEMA_FILE_SIZE_BYTES {
        return Err(WireCheckError::Io(
            file.to_path_buf(),
            format!(
                "{} bytes exceeds the {} byte schema budget",
                meta.len(),
                AstGuard::MAX_SCHEMA_FILE_SIZE_BYTES
            ),
        ));
    }
    let bytes = std::fs::read(file).map_err(io)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// At most this many findings are listed; the rest are counted.
const MAX_LISTED_FINDINGS: usize = 100;

fn render_report(report: &WireReport) -> String {
    let short_sha = report.base_sha.get(..12).unwrap_or(&report.base_sha);
    let base = match &report.base {
        BaseChoice::Explicit(b) => format!("`{b}` (`{short_sha}`)"),
        BaseChoice::MergeBase(r) => format!("merge-base of `HEAD` and `{r}` (`{short_sha}`)"),
        BaseChoice::HeadFallback => format!(
            "`HEAD` (`{short_sha}`): no `origin/HEAD`, `origin/main` or `main` ref, or no merge-base with it"
        ),
    };
    let mut out = format!(
        "\n### Wire-format check: `{}`\n- **Base**: {base}, compared with the working tree\n",
        report.rel_path
    );
    match &report.outcome {
        WireOutcome::NewFile => {
            out.push_str(
                "- **Result**: new file (absent from the base) — no breaking change possible\n",
            );
        }
        WireOutcome::Unparseable { side, reason } => {
            out.push_str(&format!(
                "- **Result**: not compared — the {side} version could not be parsed ({reason})\n"
            ));
        }
        WireOutcome::Compared(diff) => {
            if diff.breaking.is_empty() {
                out.push_str("- **Result**: no wire-format breaking change\n");
            } else {
                out.push_str(&format!(
                    "- **Result**: {} `WIRE_FORMAT_BREAKING_CHANGE`\n",
                    diff.breaking.len()
                ));
                for change in diff.breaking.iter().take(MAX_LISTED_FINDINGS) {
                    out.push_str(&format!(
                        "  - `WIRE_FORMAT_BREAKING_CHANGE` {} — `{}` #{} (L{}): {}\n",
                        change.rule.label(),
                        change.message,
                        change.number,
                        change.line,
                        change.detail
                    ));
                }
                if diff.breaking.len() > MAX_LISTED_FINDINGS {
                    out.push_str(&format!(
                        "  - … and {} more\n",
                        diff.breaking.len() - MAX_LISTED_FINDINGS
                    ));
                }
            }
            if !diff.removed_messages.is_empty() {
                let listed: Vec<String> = diff
                    .removed_messages
                    .iter()
                    .take(20)
                    .map(|m| format!("`{m}`"))
                    .collect();
                let more = diff.removed_messages.len().saturating_sub(20);
                out.push_str(&format!(
                    "- **Messages removed or renamed** (fields not compared): {}{}\n",
                    listed.join(", "),
                    if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    }
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_parsers::languages::proto::WireRule;

    /// A throwaway repository with its own `HOME` and no global/system Git
    /// config, so nothing from the developer's `~/.gitconfig` leaks in.
    struct TempRepo {
        _dir: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
    }

    impl TempRepo {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let base = dunce::canonicalize(dir.path()).expect("canon");
            let root = base.join("repo");
            let home = base.join("home");
            std::fs::create_dir_all(&root).expect("mkdir repo");
            std::fs::create_dir_all(&home).expect("mkdir home");
            let repo = Self {
                _dir: dir,
                root,
                home,
            };
            repo.git(&["init", "-q"]);
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
            repo.git(&["config", "user.email", "test@example.invalid"]);
            repo.git(&["config", "user.name", "Test"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            repo.git(&["config", "core.autocrlf", "false"]);
            repo
        }

        fn isolated_env(home: &Path) -> Vec<(OsString, OsString)> {
            vec![
                ("HOME".into(), home.as_os_str().to_owned()),
                ("USERPROFILE".into(), home.as_os_str().to_owned()),
                (
                    "XDG_CONFIG_HOME".into(),
                    home.join(".config").into_os_string(),
                ),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                (
                    "GIT_CONFIG_GLOBAL".into(),
                    home.join(".gitconfig").into_os_string(),
                ),
            ]
        }

        fn runner(&self) -> GitRunner {
            GitRunner {
                path: std::env::var_os("PATH"),
                env: Self::isolated_env(&self.home),
                timeout: GIT_COMMAND_TIMEOUT,
            }
        }

        fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(&self.root)
                .envs(Self::isolated_env(&self.home))
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let path = self.root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, content).expect("write");
            path
        }

        fn commit(&self, msg: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", msg]);
        }
    }

    const BASE_PROTO: &str = r#"syntax = "proto3";
package demo.v1;

message User {
  string id = 1;
  string email = 2;
  int32 age = 3;
  oneof contact {
    string phone = 4;
    string fax = 5;
  }
  message Address {
    string city = 1;
  }
}
"#;

    fn committed_repo() -> (TempRepo, PathBuf) {
        let repo = TempRepo::new();
        let file = repo.write("protos/user.proto", BASE_PROTO);
        repo.commit("base");
        (repo, file)
    }

    fn compared(report: &WireReport) -> &WireDiff {
        match &report.outcome {
            WireOutcome::Compared(d) => Some(d),
            _ => None,
        }
        .unwrap_or_else(|| unreachable!("expected a comparison, got {report:?}"))
    }

    fn rules(diff: &WireDiff) -> Vec<(WireRule, String, u32)> {
        diff.breaking
            .iter()
            .map(|c| (c.rule, c.message.clone(), c.number))
            .collect()
    }

    // --- The three rules -------------------------------------------------

    #[test]
    fn rule_field_number_reused_for_another_field() {
        let (repo, file) = committed_repo();
        repo.write(
            "protos/user.proto",
            &BASE_PROTO.replace("string email = 2;", "string display_name = 2;"),
        );
        let report = check_wire_format(&file, Some("main"), &repo.runner()).expect("check");
        let diff = compared(&report);
        assert_eq!(
            rules(diff),
            vec![(WireRule::FieldNumberReused, "User".to_string(), 2)]
        );
        let text = render_report(&report);
        assert!(text.contains("WIRE_FORMAT_BREAKING_CHANGE"), "{text}");
        assert!(text.contains("`main`"), "{text}");
    }

    #[test]
    fn rule_incompatible_type_and_compatible_widening() {
        let (repo, file) = committed_repo();
        // age: int32 -> int64 is wire compatible (varint group); a nested
        // message's string -> sint32 is not.
        repo.write(
            "protos/user.proto",
            &BASE_PROTO
                .replace("int32 age = 3;", "int64 age = 3;")
                .replace("string city = 1;", "sint32 city = 1;"),
        );
        let report = check_wire_format(&file, Some("main"), &repo.runner()).expect("check");
        assert_eq!(
            rules(compared(&report)),
            vec![(WireRule::IncompatibleType, "User.Address".to_string(), 1)]
        );
    }

    #[test]
    fn rule_field_deleted_without_reserved() {
        let (repo, file) = committed_repo();
        // `fax` (oneof member) deleted with nothing reserved; `age` deleted but
        // its number reserved by range; `phone` deleted but its name reserved.
        let edited = BASE_PROTO
            .replace(
                "  int32 age = 3;\n",
                "  reserved 3 to max;\n  reserved \"phone\";\n",
            )
            .replace("    string phone = 4;\n", "")
            .replace("    string fax = 5;\n", "    string pager = 6;\n");
        repo.write("protos/user.proto", &edited);
        let report = check_wire_format(&file, Some("main"), &repo.runner()).expect("check");
        // `fax` #5 falls inside `3 to max`, so it is reserved too; `pager` #6
        // is new but its number was not reserved in the base: no reuse.
        assert!(rules(compared(&report)).is_empty(), "{report:?}");

        let edited = BASE_PROTO.replace("    string fax = 5;\n", "");
        repo.write("protos/user.proto", &edited);
        let report = check_wire_format(&file, Some("main"), &repo.runner()).expect("check");
        assert_eq!(
            rules(compared(&report)),
            vec![(WireRule::DeletedWithoutReserved, "User".to_string(), 5)]
        );
    }

    // --- Base selection ----------------------------------------------------

    #[test]
    fn default_base_is_merge_base_with_main_not_the_previous_commit() {
        let (repo, file) = committed_repo();
        let base_sha = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["checkout", "-q", "-b", "feature"]);
        // Two commits on the branch: the breaking one first, then an unrelated
        // one. `HEAD~1` would miss the break; the merge-base with main keeps it.
        repo.write(
            "protos/user.proto",
            &BASE_PROTO.replace("string email = 2;", "string display_name = 2;"),
        );
        repo.commit("break");
        repo.write("README.md", "hi\n");
        repo.commit("unrelated");

        let report = check_wire_format(&file, None, &repo.runner()).expect("check");
        assert_eq!(report.base, BaseChoice::MergeBase("main"));
        assert_eq!(report.base_sha, base_sha);
        assert_eq!(
            rules(compared(&report)),
            vec![(WireRule::FieldNumberReused, "User".to_string(), 2)]
        );
        assert!(render_report(&report).contains("merge-base of `HEAD` and `main`"));
    }

    #[test]
    fn default_base_falls_back_to_head_without_main() {
        let (repo, file) = committed_repo();
        repo.git(&["branch", "-m", "main", "trunk"]);
        let report = check_wire_format(&file, None, &repo.runner()).expect("check");
        assert_eq!(report.base, BaseChoice::HeadFallback);
        assert!(compared(&report).breaking.is_empty());
    }

    // --- Edge cases ----------------------------------------------------------

    #[test]
    fn file_absent_from_base_is_new_without_breaking_change() {
        let (repo, _) = committed_repo();
        let file = repo.write(
            "protos/new.proto",
            "syntax = \"proto3\";\nmessage N { int32 a = 1; }\n",
        );
        let report = check_wire_format(&file, Some("main"), &repo.runner()).expect("check");
        assert!(matches!(report.outcome, WireOutcome::NewFile), "{report:?}");
        assert!(render_report(&report).contains("new file"));
    }

    #[test]
    fn outside_a_git_repository_is_an_explicit_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dunce::canonicalize(dir.path()).expect("canon");
        let home = base.join("home");
        std::fs::create_dir_all(&home).expect("home");
        let file = base.join("plain.proto");
        std::fs::write(&file, BASE_PROTO).expect("write");
        let runner = GitRunner {
            path: std::env::var_os("PATH"),
            env: {
                let mut env = TempRepo::isolated_env(&home);
                // Stop the upward repository search at the temp dir, in case the
                // system temp dir itself sits inside a checkout.
                env.push((
                    "GIT_CEILING_DIRECTORIES".into(),
                    base.clone().into_os_string(),
                ));
                env
            },
            timeout: GIT_COMMAND_TIMEOUT,
        };
        let err = check_wire_format(&file, Some("main"), &runner).expect_err("not a repo");
        assert!(matches!(err, WireCheckError::NotARepository(_)), "{err}");
    }

    #[test]
    fn unknown_base_is_an_explicit_error() {
        let (repo, file) = committed_repo();
        let err = check_wire_format(&file, Some("no-such-branch"), &repo.runner())
            .expect_err("unknown base");
        assert!(matches!(err, WireCheckError::BaseNotFound(_)), "{err}");
    }

    #[test]
    fn git_missing_from_path_is_an_explicit_error() {
        let (repo, file) = committed_repo();
        let empty = tempfile::tempdir().expect("empty dir");
        let runner = GitRunner {
            path: Some(empty.path().as_os_str().to_owned()),
            env: Vec::new(),
            timeout: GIT_COMMAND_TIMEOUT,
        };
        let err = check_wire_format(&file, Some("main"), &runner).expect_err("no git");
        assert!(matches!(err, WireCheckError::GitNotFound), "{err}");
        drop(repo);
    }

    #[test]
    fn option_like_or_path_like_base_is_refused_before_git_runs() {
        for bad in ["--output=/tmp/x", "-p", "main:other.proto", "", "a b"] {
            assert!(
                matches!(validate_base(bad), Err(WireCheckError::InvalidBase(..))),
                "{bad:?} must be refused"
            );
        }
        for good in ["main", "origin/main", "v1.2.0", "HEAD~3", "abc123"] {
            assert!(validate_base(good).is_ok(), "{good:?} must be accepted");
        }
        // Refused even with `git` unavailable: validation comes first.
        let runner = GitRunner {
            path: None,
            env: Vec::new(),
            timeout: GIT_COMMAND_TIMEOUT,
        };
        let err =
            check_wire_format(Path::new("x.proto"), Some("--help"), &runner).expect_err("invalid");
        assert!(matches!(err, WireCheckError::InvalidBase(..)), "{err}");
    }

    // --- Through the tool ------------------------------------------------------

    fn state_for(root: &Path) -> AppState {
        let config = mesh_core::Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n",
        )
        .expect("config");
        let audit = std::sync::Arc::new(mesh_core::AuditLogger::new_in_memory().expect("audit"));
        let rescan = std::sync::Arc::new(mesh_core::BackgroundRescanEngine::new().expect("rescan"));
        AppState::new(config, vec![root.to_path_buf()], audit, rescan)
    }

    #[test]
    fn tool_reports_breaks_and_turns_git_failures_into_tool_errors() {
        let (repo, file) = committed_repo();
        repo.write(
            "protos/user.proto",
            &BASE_PROTO.replace(
                "string email = 2;",
                "bytes email = 2;\n  repeated int32 x = 9;",
            ),
        );
        let state = state_for(&repo.root);
        let args = AnalyzeGrpcArgs {
            target: CompactStr::new(file.to_string_lossy()),
            base: Some("main".into()),
            _meta: None,
        };
        // string -> bytes is compatible, a new field is fine: no finding.
        let out = run_with_git(&args, &state, &repo.runner()).expect("tool ok");
        assert!(
            out.text.contains("no wire-format breaking change"),
            "{}",
            out.text
        );
        assert!(out.text.contains("protos/user.proto"), "{}", out.text);

        // `git` absent with an explicit base: a tool error, not a silent skip.
        let no_git = GitRunner {
            path: Some(OsString::new()),
            env: Vec::new(),
            timeout: GIT_COMMAND_TIMEOUT,
        };
        let err = run_with_git(&args, &state, &no_git)
            .err()
            .expect("tool error");
        assert!(err.1.contains("`git` was not found"), "{}", err.1);

        // A `.proto` path outside the roots is refused by the jail.
        let outside = AnalyzeGrpcArgs {
            target: CompactStr::new(repo.home.join("x.proto").to_string_lossy()),
            base: None,
            _meta: None,
        };
        std::fs::write(repo.home.join("x.proto"), "syntax = \"proto3\";\n").expect("write");
        let err = run_with_git(&outside, &state, &repo.runner())
            .err()
            .expect("jail");
        assert_eq!(err.0, -32602, "{}", err.1);
    }
}
