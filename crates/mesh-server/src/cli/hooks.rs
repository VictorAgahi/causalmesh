use mesh_core::{strip_workspace_root_prefix, Config};
use std::fs;

pub struct HooksCommand;

impl HooksCommand {
    /// Whether `[engines.policy] enforce_git_hooks` permits installing the OS-level
    /// pre-commit hook. No `[engines.policy]` section at all defaults to on, matching
    /// `PolicyConfig`'s own `#[serde(default = "default_true")]`.
    pub fn is_enabled(config: &Config) -> bool {
        config
            .engines
            .policy
            .as_ref()
            .is_none_or(|p| p.enforce_git_hooks)
    }

    pub fn run(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
        let cur_dir = std::env::current_dir()?;
        let git_hooks_dir = cur_dir.join(".git").join("hooks");

        if !git_hooks_dir.exists() {
            eprintln!(
                "✖ Error: .git/hooks directory not found. Please run inside a Git repository root."
            );
            return Ok(());
        }

        let pre_commit_path = git_hooks_dir.join("pre-commit");
        let hook_script = Self::build_hook_script(config);

        fs::write(&pre_commit_path, hook_script)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&pre_commit_path, fs::Permissions::from_mode(0o755));
        }

        eprintln!("✔ Successfully installed MeshMCP pre-commit hook in .git/hooks/pre-commit");
        eprintln!(
            "✔ Contract-first cascade CI active governance physically enforced at the OS level."
        );

        Ok(())
    }

    /// Escapes a literal path segment for safe embedding in a `grep -E`
    /// (POSIX extended regex) alternation — a configured `proto_dirs` entry
    /// is a filesystem path, not a regex, and may legitimately contain
    /// characters (`.`, `+`, etc.) that are metacharacters in ERE syntax.
    fn escape_ere(s: &str) -> String {
        const SPECIAL: &str = ".^$*+?()[]{}|\\";
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if SPECIAL.contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }

    /// Builds the pre-commit hook script, anchoring the "contract-first
    /// cascade" check on the user's own configured `[engines.contracts.grpc]
    /// proto_dirs` — not a bare `*.proto` extension match. Matching every
    /// `.proto` file in the repo (a fixture, a test, an unrelated vendored
    /// schema) against every staged source file blocks commits that have
    /// nothing to do with the actual contract boundary; anchoring on the
    /// directories the user declared as contracts keeps the check meaningful
    /// without hardcoding a directory name like "proto-registry" back in.
    /// With no `proto_dirs` configured at all, there is no honest anchor to
    /// enforce against, so the cross-boundary check is skipped entirely
    /// rather than guessing.
    fn build_hook_script(config: &Config) -> String {
        let proto_dirs: Vec<String> = config
            .engines
            .contracts
            .as_ref()
            .and_then(|c| c.grpc.as_ref())
            .map(|g| {
                g.proto_dirs
                    .iter()
                    .map(|d| strip_workspace_root_prefix(d))
                    .filter(|d| !d.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let contract_check = if proto_dirs.is_empty() {
            String::from("# No [engines.contracts.grpc] proto_dirs configured — no honest anchor\n# to enforce a contract-first boundary against, so this check is skipped.\nexit 0\n")
        } else {
            let anchor = proto_dirs
                .iter()
                .map(|d| format!("^{}/", Self::escape_ere(d)))
                .collect::<Vec<_>>()
                .join("|");
            format!(
                r#"HAS_CONTRACTS=$(echo "$STAGED_FILES" | grep -E '\.proto$' | grep -E '{anchor}' || true)
HAS_SERVICES=$(echo "$STAGED_FILES" | grep -E '\.(go|rs|java|kt|ts|tsx|py|cpp|cs|php|rb|swift|scala)$' || true)

if [ -n "$HAS_CONTRACTS" ] && [ -n "$HAS_SERVICES" ]; then
    echo "🛑 [MeshMCP GOVERNANCE BLOCKED: CONTRACT_FIRST_CASCADE_CI]"
    echo "Cross-boundary mutation detected: Interface contracts (*.proto) must be committed and CI-propagated BEFORE mutating service implementations."
    echo "👉 Required Action: Unstage service implementation files and commit contract definitions exclusively."
    exit 1
fi

exit 0
"#
            )
        };

        format!(
            r#"#!/usr/bin/env bash
# MeshMCP Pre-Commit Governance Hook (RFC-001 Commandment 6)
set -e

STAGED_FILES=$(git diff --cached --name-only)

# Contract-first cascade CI verification:
# Interface contracts under the repo's own configured proto_dirs must be
# committed and CI-propagated before modifying dependent service code.
{contract_check}"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforce_git_hooks_false_disables_install() {
        let config = Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n\n[engines.policy]\nenforce_git_hooks = false\n",
        )
        .expect("config");
        assert!(!HooksCommand::is_enabled(&config));
    }

    #[test]
    fn enforce_git_hooks_true_by_default() {
        let config =
            Config::load_from_str("[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n")
                .expect("config");
        assert!(HooksCommand::is_enabled(&config));
    }

    /// Regression test for the ultrareview finding on PR #6: with no
    /// `proto_dirs` configured, the generated hook must not fall back to
    /// matching every `*.proto` file in the repo (which would block any
    /// commit touching an unrelated `.proto` fixture alongside any source
    /// file) — it must skip the cross-boundary check entirely.
    #[test]
    fn no_proto_dirs_configured_skips_the_contract_check() {
        let config =
            Config::load_from_str("[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n")
                .expect("config");
        let script = HooksCommand::build_hook_script(&config);
        assert!(
            !script.contains("HAS_CONTRACTS"),
            "no anchor to enforce against — the contract-first check must be \
             skipped, not fall back to matching every .proto file, got:\n{script}"
        );
    }

    /// Regression test: with `proto_dirs` configured, the generated hook
    /// must anchor `HAS_CONTRACTS` on those specific directories (not a bare
    /// `*.proto` extension match), so a `.proto` file outside the declared
    /// contract boundary doesn't block an unrelated commit.
    #[test]
    fn configured_proto_dirs_anchor_the_contract_check() {
        let config = Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n\n[engines.contracts.grpc]\nproto_dirs = [\"${workspace_root}/pb\"]\n",
        )
        .expect("config");
        let script = HooksCommand::build_hook_script(&config);
        assert!(
            script.contains("HAS_CONTRACTS"),
            "a configured proto_dirs must produce an anchored contract check, got:\n{script}"
        );
        assert!(
            script.contains("^pb/"),
            "the anchor must be the resolved, workspace-root-stripped proto_dirs \
             entry, got:\n{script}"
        );
    }

    #[test]
    fn escape_ere_escapes_regex_metacharacters() {
        assert_eq!(HooksCommand::escape_ere("proto.v1"), "proto\\.v1");
        assert_eq!(HooksCommand::escape_ere("a+b"), "a\\+b");
        assert_eq!(HooksCommand::escape_ere("plain"), "plain");
    }
}
