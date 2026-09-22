use crate::types::CompactStr;
use std::borrow::Cow;
use std::collections::HashMap;

/// PropertyRegistry for configuration flattening, placeholder resolution, and active secret masking.
#[derive(Debug, Clone, Default)]
pub struct PropertyRegistry {
    flat_properties: HashMap<CompactStr, CompactStr>,
    redacted_count: usize,
}

impl PropertyRegistry {
    pub const SECRET_PATTERNS: &'static [&'static str] = &[
        "password",
        "secret",
        "token",
        "credential",
        "key",
        "auth",
        "private",
        "jwt",
        "apikey",
        "cert",
        "passphrase",
    ];

    pub const REDACTED_PLACEHOLDER: &'static str = "[REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]";

    pub fn new() -> Self {
        Self {
            flat_properties: HashMap::new(),
            redacted_count: 0,
        }
    }

    /// Absorbs another registry (later keys win) — lets per-file registries be
    /// built in parallel and folded into one.
    pub fn merge(&mut self, other: PropertyRegistry) {
        self.flat_properties.extend(other.flat_properties);
        self.redacted_count += other.redacted_count;
    }

    #[inline]
    pub fn redacted_count(&self) -> usize {
        self.redacted_count
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.flat_properties.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.flat_properties.is_empty()
    }

    #[inline]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.flat_properties.get(key).map(|v| v.as_str())
    }

    pub fn insert_sanitized(&mut self, key: &str, raw_val: &str) {
        let lower_key = key.to_lowercase();
        let is_sensitive = Self::SECRET_PATTERNS
            .iter()
            .any(|&pattern| lower_key.contains(pattern));

        let sanitized_value = if is_sensitive {
            self.redacted_count += 1;
            CompactStr::new(Self::REDACTED_PLACEHOLDER)
        } else {
            CompactStr::new(raw_val)
        };

        self.flat_properties
            .insert(CompactStr::new(key), sanitized_value);
    }

    pub fn resolve_placeholder<'a>(&'a self, raw: &'a str) -> Cow<'a, str> {
        if !raw.starts_with("${") || !raw.ends_with('}') {
            return Cow::Borrowed(raw);
        }

        let inner = &raw[2..raw.len() - 1];
        let (key, default_val) = match inner.split_once(':') {
            Some((k, def)) => (k.trim(), Some(def.trim())),
            None => (inner.trim(), None),
        };

        if let Some(val) = self.flat_properties.get(key) {
            Cow::Borrowed(val.as_str())
        } else if let Some(def) = default_val {
            let lower_key = key.to_lowercase();
            if Self::SECRET_PATTERNS.iter().any(|&p| lower_key.contains(p)) {
                Cow::Borrowed(Self::REDACTED_PLACEHOLDER)
            } else {
                Cow::Borrowed(def)
            }
        } else {
            Cow::Borrowed(raw)
        }
    }

    /// Ingests a standard Java properties formatted string
    pub fn ingest_properties_str(&mut self, content: &str) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                continue;
            }

            let separator = if let Some(idx) = trimmed.find('=') {
                Some((idx, '='))
            } else {
                trimmed.find(':').map(|idx| (idx, ':'))
            };

            if let Some((idx, _)) = separator {
                let key = trimmed[..idx].trim();
                let val = trimmed[idx + 1..].trim();
                if !key.is_empty() {
                    self.insert_sanitized(key, val);
                }
            }
        }
    }

    /// Ingests YAML configuration and flattens nested keys with dot notation
    pub fn ingest_yaml_str(&mut self, content: &str) -> Result<(), serde_yaml::Error> {
        let value: serde_yaml::Value = serde_yaml::from_str(content)?;
        self.flatten_yaml_value("", &value);
        Ok(())
    }

    fn flatten_yaml_value(&mut self, prefix: &str, value: &serde_yaml::Value) {
        match value {
            serde_yaml::Value::Mapping(map) => {
                for (k, v) in map {
                    if let Some(k_str) = k.as_str() {
                        let new_prefix = if prefix.is_empty() {
                            k_str.to_string()
                        } else {
                            format!("{prefix}.{k_str}")
                        };
                        self.flatten_yaml_value(&new_prefix, v);
                    }
                }
            }
            serde_yaml::Value::String(s) => {
                self.insert_sanitized(prefix, s);
            }
            serde_yaml::Value::Number(n) => {
                self.insert_sanitized(prefix, &n.to_string());
            }
            serde_yaml::Value::Bool(b) => {
                self.insert_sanitized(prefix, &b.to_string());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_redaction_properties() {
        let mut registry = PropertyRegistry::new();
        registry.ingest_properties_str(
            r#"
# Sample properties
app.name=MeshService
spring.datasource.password=super_secret_db_password
jwt.token.secret=my_super_jwt_secret
server.port=8080
"#,
        );

        assert_eq!(registry.get("app.name"), Some("MeshService"));
        assert_eq!(registry.get("server.port"), Some("8080"));
        assert_eq!(
            registry.get("spring.datasource.password"),
            Some(PropertyRegistry::REDACTED_PLACEHOLDER)
        );
        assert_eq!(
            registry.get("jwt.token.secret"),
            Some(PropertyRegistry::REDACTED_PLACEHOLDER)
        );
        assert_eq!(registry.redacted_count(), 2);
    }

    #[test]
    fn test_placeholder_resolution() {
        let mut registry = PropertyRegistry::new();
        registry.insert_sanitized("db.host", "localhost");
        registry.insert_sanitized("db.password", "secret123");

        assert_eq!(
            registry.resolve_placeholder("${db.host:127.0.0.1}"),
            "localhost"
        );
        assert_eq!(
            registry.resolve_placeholder("${db.password:pass}"),
            PropertyRegistry::REDACTED_PLACEHOLDER
        );
        assert_eq!(registry.resolve_placeholder("${db.port:5432}"), "5432");
        assert_eq!(
            registry.resolve_placeholder("${app.api_key:my_secret_token}"),
            PropertyRegistry::REDACTED_PLACEHOLDER
        );
    }

    #[test]
    fn test_yaml_flattening() {
        let mut registry = PropertyRegistry::new();
        let yaml = r#"
server:
  port: 9090
spring:
  datasource:
    url: jdbc:postgresql://localhost:5432/mesh
    password: my_db_password
"#;
        registry.ingest_yaml_str(yaml).expect("valid yaml");
        assert_eq!(registry.get("server.port"), Some("9090"));
        assert_eq!(
            registry.get("spring.datasource.password"),
            Some(PropertyRegistry::REDACTED_PLACEHOLDER)
        );
    }
}
