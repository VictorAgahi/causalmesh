use mesh_core::Config;
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

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let cur_dir = std::env::current_dir()?;
        let git_hooks_dir = cur_dir.join(".git").join("hooks");

        if !git_hooks_dir.exists() {
            eprintln!(
                "✖ Error: .git/hooks directory not found. Please run inside a Git repository root."
            );
            return Ok(());
        }

        let pre_commit_path = git_hooks_dir.join("pre-commit");

        let hook_script = r#"#!/usr/bin/env bash
# MeshMCP Pre-Commit Governance Hook (RFC-001 Commandment 6)
set -e

STAGED_FILES=$(git diff --cached --name-only)

# Contract-first cascade CI verification:
# Interface contracts (.proto) must be committed and CI-propagated before modifying dependent service code
HAS_CONTRACTS=$(echo "$STAGED_FILES" | grep -E '\.(proto)$' || true)
HAS_SERVICES=$(echo "$STAGED_FILES" | grep -E '\.(go|rs|java|kt|ts|tsx|py|cpp|cs|php|rb|swift|scala)$' || true)

if [ -n "$HAS_CONTRACTS" ] && [ -n "$HAS_SERVICES" ]; then
    echo "🛑 [MeshMCP GOVERNANCE BLOCKED: CONTRACT_FIRST_CASCADE_CI]"
    echo "Cross-boundary mutation detected: Interface contracts (*.proto) must be committed and CI-propagated BEFORE mutating service implementations."
    echo "👉 Required Action: Unstage service implementation files and commit contract definitions exclusively."
    exit 1
fi

exit 0
"#;

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
}
