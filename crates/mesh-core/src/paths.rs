//! Shared resolution for `~/.cache/mesh-mcp/`, the base directory every persistent,
//! machine-local mesh-mcp artifact lives under (the audit db, the persistent index cache, and
//! `meshd`'s auto-spawn logs). Factored out so `$HOME`/`$USERPROFILE`/temp-dir fallback logic
//! exists in exactly one place instead of being re-derived per artifact.

use std::path::PathBuf;

/// `~/.cache/mesh-mcp` (or `%USERPROFILE%\.cache\mesh-mcp` on Windows when `HOME` isn't set),
/// falling back to `std::env::temp_dir()/mesh-mcp` when neither environment variable is set.
pub fn mesh_cache_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        PathBuf::from(home).join(".cache").join("mesh-mcp")
    } else {
        std::env::temp_dir().join("mesh-mcp")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ends_with_mesh_mcp() {
        assert!(mesh_cache_dir().ends_with("mesh-mcp"));
    }
}
