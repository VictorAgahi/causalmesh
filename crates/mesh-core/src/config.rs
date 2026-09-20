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
pub fn expand_roots(raw_roots: &[String], base_dir: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let mut resolved = Vec::new();

    for raw in raw_roots {
        let expanded =
            shellexpand::env_with_context(raw, |var| Ok(std::env::var(var).ok().map(Cow::Owned)))
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
        expand_roots(&self.workspace.roots, base_dir)
    }
}
