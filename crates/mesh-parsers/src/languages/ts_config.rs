use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// High-performance resolver for TypeScript / JavaScript `compilerOptions.paths` alias mappings.
#[derive(Debug, Clone, Default)]
pub struct TsConfigResolver {
    /// Mappings from path alias prefix (e.g. "@/") to relative target prefix (e.g. "src/")
    pub path_mappings: HashMap<String, String>,
}

#[derive(Deserialize)]
struct TsConfigJson {
    #[serde(rename = "compilerOptions")]
    compiler_options: Option<CompilerOptionsJson>,
}

#[derive(Deserialize)]
struct CompilerOptionsJson {
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,
    paths: Option<HashMap<String, Vec<String>>>,
}

impl TsConfigResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_file(path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        Self::load_from_str(&content)
    }

    pub fn load_from_str(content: &str) -> Option<Self> {
        let cleaned = Self::strip_json_comments(content);
        let parsed: TsConfigJson = serde_json::from_str(&cleaned).ok()?;
        let options = parsed.compiler_options?;
        let paths = options.paths?;
        let base_url = options.base_url.unwrap_or_else(|| ".".to_string());

        let mut path_mappings = HashMap::new();
        for (pattern, targets) in paths {
            let Some(first_target) = targets.first() else {
                continue;
            };
            let prefix = pattern.trim_end_matches('*').to_string();
            let mut target_prefix = first_target.trim_end_matches('*').to_string();
            if target_prefix.starts_with("./") {
                target_prefix = target_prefix[2..].to_string();
            }
            let full_target = if base_url == "." || base_url == "./" || base_url.is_empty() {
                target_prefix
            } else {
                format!("{}/{}", base_url.trim_end_matches('/'), target_prefix)
            };
            path_mappings.insert(prefix, full_target);
        }

        Some(Self { path_mappings })
    }

    /// Strips single-line (`//`) and multi-line (`/* ... */`) comments from JSONC/TSConfig files.
    fn strip_json_comments(input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let len = bytes.len();
        let mut i = 0;
        let mut in_string = false;

        while i < len {
            let b = bytes[i];
            if b == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
                in_string = !in_string;
                result.push(b as char);
                i += 1;
                continue;
            }
            if !in_string {
                if b == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
                    i += 2;
                    while i < len && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
                    i += 2;
                    while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(len);
                    continue;
                }
            }
            result.push(b as char);
            i += 1;
        }

        result
    }

    pub fn resolve_alias(&self, import_target: &str) -> Option<String> {
        for (prefix, target_prefix) in &self.path_mappings {
            if import_target.starts_with(prefix) {
                let suffix = &import_target[prefix.len()..];
                return Some(format!("{target_prefix}{suffix}"));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tsconfig_resolver_parsing() {
        let jsonc = r#"
        {
          // TSConfig comment
          "compilerOptions": {
            "baseUrl": "./",
            /* Multi-line comment */
            "paths": {
              "@/*": ["src/*"],
              "@components/*": ["src/components/*"]
            }
          }
        }
        "#;
        let resolver = TsConfigResolver::load_from_str(jsonc).expect("parse tsconfig");
        assert_eq!(
            resolver.resolve_alias("@/services/User"),
            Some("src/services/User".to_string())
        );
        assert_eq!(
            resolver.resolve_alias("@components/Button"),
            Some("src/components/Button".to_string())
        );
        assert_eq!(resolver.resolve_alias("express"), None);
    }
}
