//! Refuses a degenerate workspace root.
//!
//! An MCP client launched with `cwd = /` (or the user's home directory) and no
//! `mesh-mcp.toml` makes the default single-root config resolve to that
//! directory: the daemon would be spawned for, and try to scan, the whole
//! filesystem or the whole home. One helper decides this for `run`, the daemon
//! spawn, `meshd` itself and `doctor`, so they cannot drift apart.

use std::path::{Component, Path, PathBuf};

/// Why a directory is refused as a workspace root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegenerateKind {
    /// `/` on Unix.
    FilesystemRoot,
    /// `C:\` and friends on Windows.
    DriveRoot,
    /// The user's home directory.
    Home,
}

impl DegenerateKind {
    fn describe(self) -> &'static str {
        match self {
            Self::FilesystemRoot => "the filesystem root",
            Self::DriveRoot => "a drive root",
            Self::Home => "the user's home directory",
        }
    }
}

/// The error of [`guard_workspace_root`]: names the directory that was
/// resolved and how to get out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegenerateRootError {
    /// The directory the workspace resolved to, as given (not canonicalized).
    pub dir: PathBuf,
    pub kind: DegenerateKind,
}

impl std::fmt::Display for DegenerateRootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to index `{}`: the workspace resolved to {} because no mesh-mcp.toml was \
             found there (the MCP client was probably launched with cwd = `{}`). Nothing was \
             scanned and no daemon was started. Pass `-c <path/to/mesh-mcp.toml>` (a config that \
             lists explicit `roots`), or start the MCP server from inside your project directory.",
            self.dir.display(),
            self.kind.describe(),
            self.dir.display(),
        )
    }
}

impl std::error::Error for DegenerateRootError {}

/// The current user's home directory from the environment (`HOME`, else
/// `USERPROFILE`); `None` when neither is set or both are empty.
pub fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Canonical form used for the comparison: symlinks resolved when the path
/// exists, otherwise the path as given. A trailing separator never matters
/// (`Path` components ignore it).
fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `path` has a root and nothing else: `/`, `C:\`, `\\?\C:\`.
fn is_bare_root(path: &Path) -> bool {
    let mut saw_root = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) => {}
            Component::RootDir => saw_root = true,
            _ => return false,
        }
    }
    saw_root
}

/// Classifies `dir` against `home` (pass [`home_dir`] in production). Both are
/// canonicalized, so a symlink to `/` or to the home directory, a `..` detour
/// and a trailing slash all classify like the directory they name.
pub fn classify_workspace_root(dir: &Path, home: Option<&Path>) -> Option<DegenerateKind> {
    let dir = canonical(dir);
    if is_bare_root(&dir) {
        return Some(if cfg!(windows) || dir.to_string_lossy().contains(':') {
            DegenerateKind::DriveRoot
        } else {
            DegenerateKind::FilesystemRoot
        });
    }
    let home = canonical(home?);
    if is_bare_root(&home) {
        // A "home" of `/` is not a home directory worth special-casing here;
        // the filesystem-root rule above already covers `/` itself.
        return None;
    }
    (dir == home).then_some(DegenerateKind::Home)
}

/// Refuses `base_dir` as a workspace when it is a degenerate root and no config
/// file pinned the roots. `config_pins_roots` is true when a config file was
/// found (`-c`, `.agents/mesh-mcp.toml` or `mesh-mcp.toml`): its `roots` list is
/// the user's explicit choice and is always honoured, even in a home directory.
pub fn guard_workspace_root(
    base_dir: &Path,
    config_pins_roots: bool,
    home: Option<&Path>,
) -> Result<(), DegenerateRootError> {
    if config_pins_roots {
        return Ok(());
    }
    match classify_workspace_root(base_dir, home) {
        None => Ok(()),
        Some(kind) => Err(DegenerateRootError {
            dir: dunce::canonicalize(base_dir).unwrap_or_else(|_| base_dir.to_path_buf()),
            kind,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn filesystem_root_is_refused_in_every_spelling() {
        for spelling in ["/", "//", "/.", "/../"] {
            let err = guard_workspace_root(Path::new(spelling), false, None).expect_err(spelling);
            assert_eq!(err.kind, DegenerateKind::FilesystemRoot, "{spelling}");
            assert_eq!(err.dir, Path::new("/"), "{spelling}");
        }
    }

    #[test]
    fn home_is_refused_with_or_without_a_trailing_slash() {
        let home = tempfile::tempdir().expect("home");
        let canon = dunce::canonicalize(home.path()).expect("canon");
        for spelling in [
            canon.clone(),
            PathBuf::from(format!("{}/", canon.display())),
            canon.join("."),
        ] {
            let err = guard_workspace_root(&spelling, false, Some(&canon))
                .expect_err(&spelling.display().to_string());
            assert_eq!(err.kind, DegenerateKind::Home);
        }
        // A project below home is fine.
        let project = canon.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        assert!(guard_workspace_root(&project, false, Some(&canon)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_resolved_before_comparing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir");
        let to_home = tmp.path().join("to-home");
        let to_root = tmp.path().join("to-root");
        std::os::unix::fs::symlink(&home, &to_home).expect("symlink");
        std::os::unix::fs::symlink("/", &to_root).expect("symlink");
        // The home given through a different symlink than the cwd.
        let home_alias = tmp.path().join("home-alias");
        std::os::unix::fs::symlink(&home, &home_alias).expect("symlink");

        let err = guard_workspace_root(&to_home, false, Some(&home_alias)).expect_err("home");
        assert_eq!(err.kind, DegenerateKind::Home);
        let err = guard_workspace_root(&to_root, false, Some(&home)).expect_err("root");
        assert_eq!(err.kind, DegenerateKind::FilesystemRoot);
        assert_eq!(err.dir, Path::new("/"));
    }

    #[test]
    fn a_config_that_pins_roots_is_always_honoured() {
        let home = tempfile::tempdir().expect("home");
        assert!(guard_workspace_root(home.path(), true, Some(home.path())).is_ok());
        #[cfg(unix)]
        assert!(guard_workspace_root(Path::new("/"), true, None).is_ok());
    }

    #[test]
    fn without_a_known_home_only_the_filesystem_root_is_degenerate() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(guard_workspace_root(dir.path(), false, None).is_ok());
        // A missing directory is not degenerate either (nothing to compare).
        assert!(guard_workspace_root(&dir.path().join("missing"), false, None).is_ok());
    }

    #[test]
    fn the_error_names_the_resolved_directory_and_the_remedy() {
        let home = tempfile::tempdir().expect("home");
        let canon = dunce::canonicalize(home.path()).expect("canon");
        let err = guard_workspace_root(&canon, false, Some(&canon)).expect_err("home");
        let text = err.to_string();
        assert!(text.contains(&canon.display().to_string()), "{text}");
        assert!(text.contains("-c <path/to/mesh-mcp.toml>"), "{text}");
        assert!(text.contains("home directory"), "{text}");
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_root_is_refused() {
        let err = guard_workspace_root(Path::new("C:\\"), false, None).expect_err("drive");
        assert_eq!(err.kind, DegenerateKind::DriveRoot);
    }
}
