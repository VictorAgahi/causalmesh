use crate::types::CompactStr;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to parse TOML configuration: {0}")]
    ParseError(#[from] toml::de::Error),

    #[error("Failed to read configuration file at {0}: {1}")]
    IoError(PathBuf, #[source] std::io::Error),

    #[error("Environment variable expansion failed: {0}")]
    EnvExpansionFailed(String),

    #[error("Invalid glob pattern '{0}': {1}")]
    GlobPatternError(String, String),

    #[error("Glob path resolution error: {0}")]
    GlobPathError(String),

    #[error("Invalid root path '{0}': {1}")]
    InvalidRootPath(String, #[source] std::io::Error),

    #[error("Prohibited root directory rejected: '{0}'")]
    ProhibitedRootConfigured(String),

    #[error("No valid root directories configured or resolved")]
    NoValidRootsConfigured,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub engines: EnginesConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default = "default_workspace_root")]
    pub workspace_root: String,
    pub roots: Vec<String>,
    #[serde(default = "default_exclude_patterns")]
    pub exclude_patterns: Vec<String>,
    #[serde(default)]
    pub mount_aliases: HashMap<String, String>,
}

fn default_version() -> String {
    "2.9.0".to_string()
}

fn default_workspace_root() -> String {
    ".".to_string()
}

fn default_exclude_patterns() -> Vec<String> {
    vec![
        "**/.env*".to_string(),
        "**/secrets/**".to_string(),
        "**/*.pem".to_string(),
        "**/*.key".to_string(),
        "**/id_rsa*".to_string(),
        "**/id_ed25519*".to_string(),
        "**/id_ecdsa*".to_string(),
        "**/.npmrc".to_string(),
        "**/.pypirc".to_string(),
        "**/.dockercfg".to_string(),
        "**/.docker/**".to_string(),
        "**/.kube/**".to_string(),
        "**/*.jks".to_string(),
        "**/*.p12".to_string(),
        "**/*.pfx".to_string(),
        "**/*.keystore".to_string(),
        "**/node_modules/**".to_string(),
        "**/target/**".to_string(),
        "**/build/**".to_string(),
        "**/.venv/**".to_string(),
        "**/.git/**".to_string(),
    ]
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnginesConfig {
    #[serde(default)]
    pub docs: Option<DocsConfig>,
    #[serde(default)]
    pub contracts: Option<ContractsConfig>,
    #[serde(default)]
    pub policy: Option<PolicyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    #[serde(default)]
    pub stop_words: Vec<String>,
    #[serde(default = "default_exact_phrase_boost")]
    pub exact_phrase_boost: u32,
    #[serde(default = "default_true")]
    pub fuzzy_fallback: bool,
    #[serde(default = "default_true")]
    pub sanitize_prompt_injections: bool,
}

fn default_true() -> bool {
    true
}

fn default_exact_phrase_boost() -> u32 {
    60
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub grpc: Option<GrpcConfig>,
    #[serde(default)]
    pub spring: Option<SpringConfig>,
    #[serde(default)]
    pub openapi: Option<OpenApiConfig>,
    #[serde(default)]
    pub asyncapi: Option<AsyncApiConfig>,
    #[serde(default)]
    pub patterns: Vec<CustomPatternConfig>,
}

fn default_group_1() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomPatternConfig {
    pub name: String,
    pub kind: PatternKind,
    #[serde(default)]
    pub file_pattern: Option<String>,
    pub regex: String,
    #[serde(default = "default_group_1")]
    pub target_group: usize,
    #[serde(default)]
    pub consumer_group: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    TopicProducer,
    TopicConsumer,
    Saga,
    Rpc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcConfig {
    #[serde(default)]
    pub proto_dirs: Vec<String>,
    #[serde(default)]
    pub controller_annotations: Vec<String>,
    #[serde(default = "default_true")]
    pub canonical_fqcn_projection: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpringConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub property_files: Vec<String>,
    #[serde(default = "default_true")]
    pub resolve_placeholders: bool,
    #[serde(default = "default_true")]
    pub auto_redact_secrets: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenApiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub spec_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsyncApiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub spec_files: Vec<String>,
    #[serde(default = "default_true")]
    pub infer_string_topics: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadGovernanceMode {
    #[default]
    AllowAll,
    AuditWarn,
    EnforceRefusal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub enforce_git_hooks: bool,
    #[serde(default = "default_true")]
    pub cryptographic_audit_trail: bool,
    #[serde(default)]
    pub read_governance_mode: ReadGovernanceMode,
    #[serde(default)]
    pub stop_rules: HashMap<CompactStr, String>,
    #[serde(default)]
    pub skills: HashMap<CompactStr, String>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enforce_git_hooks: true,
            cryptographic_audit_trail: true,
            read_governance_mode: ReadGovernanceMode::AllowAll,
            stop_rules: HashMap::new(),
            skills: HashMap::new(),
        }
    }
}

const PROHIBITED_ROOTS: &[&str] = &[
    "/",
    "/etc",
    "/var",
    "/tmp",
    "/Users",
    "/home",
    "/root",
    "C:\\",
    "C:\\Windows",
    "C:\\Users",
    "C:\\Program Files",
];

fn validate_not_prohibited(path: &Path) -> Result<(), ConfigError> {
    let path_str = path.to_string_lossy();
    for &prohibited in PROHIBITED_ROOTS {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let match_cond = path_str.to_lowercase() == prohibited.to_lowercase();
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let match_cond = path_str == prohibited;

        if match_cond {
            return Err(ConfigError::ProhibitedRootConfigured(path_str.into_owned()));
        }
    }
    Ok(())
}

/// Expands environment variables and globs in root configurations.
///
/// `workspace_root` is the raw `[workspace] workspace_root` value (itself possibly
/// containing an OS env var reference, e.g. `${WORKSPACE_ROOT:-.}`). It is resolved
/// once up front so that `${workspace_root}` placeholders inside each root string
/// resolve to it, in addition to real OS environment variables.
pub fn expand_roots(
    raw_roots: &[String],
    base_dir: &Path,
    workspace_root: &str,
) -> Result<Vec<PathBuf>, ConfigError> {
    let mut resolved = Vec::new();

    let resolved_workspace_root = shellexpand::env_with_context(workspace_root, |var| {
        Ok(std::env::var(var).ok().map(Cow::Owned))
    })
    .map_err(|e: shellexpand::LookupError<std::convert::Infallible>| {
        ConfigError::EnvExpansionFailed(e.to_string())
    })?
    .into_owned();

    for raw in raw_roots {
        let expanded = shellexpand::env_with_context(raw, |var| {
            if var == "workspace_root" {
                return Ok(Some(Cow::Borrowed(resolved_workspace_root.as_str())));
            }
            Ok(std::env::var(var).ok().map(Cow::Owned))
        })
        .map_err(|e: shellexpand::LookupError<std::convert::Infallible>| {
            ConfigError::EnvExpansionFailed(e.to_string())
        })?;

        let path_pattern = if Path::new(expanded.as_ref()).is_absolute() {
            expanded.into_owned()
        } else {
            base_dir
                .join(expanded.as_ref())
                .to_string_lossy()
                .into_owned()
        };

        if path_pattern.contains('*') || path_pattern.contains('?') {
            let glob_entries = glob::glob_with(
                &path_pattern,
                glob::MatchOptions {
                    case_sensitive: false,
                    require_literal_separator: true,
                    require_literal_leading_dot: true,
                },
            )
            .map_err(|e| ConfigError::GlobPatternError(path_pattern.clone(), e.to_string()))?;

            for entry in glob_entries {
                let path = entry.map_err(|e| ConfigError::GlobPathError(e.to_string()))?;
                if path.is_dir() {
                    let canonical = dunce::canonicalize(&path)
                        .map_err(|e| ConfigError::InvalidRootPath(path.display().to_string(), e))?;
                    validate_not_prohibited(&canonical)?;
                    if !resolved.contains(&canonical) {
                        resolved.push(canonical);
                    }
                }
            }
        } else {
            let path = PathBuf::from(path_pattern);
            if path.exists() && path.is_dir() {
                let canonical = dunce::canonicalize(&path)
                    .map_err(|e| ConfigError::InvalidRootPath(path.display().to_string(), e))?;
                validate_not_prohibited(&canonical)?;
                if !resolved.contains(&canonical) {
                    resolved.push(canonical);
                }
            }
        }
    }

    if resolved.is_empty() {
        return Err(ConfigError::NoValidRootsConfigured);
    }

    Ok(resolved)
}

impl Config {
    /// Rewrites relative `[engines.policy.skills]` paths so they resolve against the
    /// config file's own directory rather than the process working directory.
    ///
    /// Without this, a skill would be found by `doctor` (run from the repo root) but
    /// not by the server (spawned by an IDE with an arbitrary cwd), or vice versa.
    pub fn resolve_skill_paths(&mut self, base_dir: &Path) {
        let Some(policy) = self.engines.policy.as_mut() else {
            return;
        };
        for path in policy.skills.values_mut() {
            let p = Path::new(path.as_str());
            if p.is_absolute() || p.exists() {
                continue;
            }
            let joined = base_dir.join(p);
            if joined.exists() {
                *path = joined.to_string_lossy().into_owned();
            }
        }
    }
    pub fn load_from_str(content: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(content)?;
        Ok(cfg)
    }

    pub fn load_from_file(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::IoError(path.to_path_buf(), e))?;
        Self::load_from_str(&content)
    }

    pub fn resolve_allowed_roots(&self, base_dir: &Path) -> Result<Vec<PathBuf>, ConfigError> {
        expand_roots(
            &self.workspace.roots,
            base_dir,
            &self.workspace.workspace_root,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_paths_resolve_against_config_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agents = tmp.path().join(".agents");
        std::fs::create_dir_all(agents.join("skills")).expect("mkdir");
        std::fs::write(agents.join("skills/proto.md"), "# Proto").expect("write");

        let mut cfg = Config::load_from_str(
            r#"
[workspace]
name = "t"
version = "0"
roots = ["."]

[engines.policy.skills]
"proto-registry" = "skills/proto.md"
"absent" = "skills/nope.md"
"#,
        )
        .expect("config");

        cfg.resolve_skill_paths(&agents);
        let skills = &cfg.engines.policy.as_ref().expect("policy").skills;

        // An existing file is rewritten to a path that resolves from any cwd.
        let resolved = skills.get("proto-registry").expect("key");
        assert!(std::path::Path::new(resolved).exists(), "{resolved}");
        // A path that resolves nowhere is left untouched so doctor reports it verbatim.
        assert_eq!(
            skills.get("absent").map(String::as_str),
            Some("skills/nope.md")
        );
    }
}
