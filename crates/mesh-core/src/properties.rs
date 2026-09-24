use crate::types::CompactStr;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Matches candidate files against `[engines.contracts.spring].property_files` globs, scoping
/// which `.properties` / `.yml` / `.yaml` files are treated as Spring property sources. An
/// absent or empty pattern list means "match everything" — the caller should skip constructing
/// a matcher in that case and keep the pre-existing unscoped behaviour.
///
/// Pattern normalization mirrors `crate::crawler::ExcludeMatcher`: a pattern without a leading
/// `**/` or `/` is anchored to match at any depth in the tree.
#[derive(Debug, Clone)]
pub struct PropertySourceMatcher {
    set: GlobSet,
}

impl PropertySourceMatcher {
    pub fn compile(patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        for raw in patterns {
            let pat = raw.trim();
            if pat.is_empty() {
                continue;
            }
            let anchored = if pat.starts_with("**/") || pat.starts_with('/') {
                pat.to_string()
            } else {
                format!("**/{pat}")
            };
            match Glob::new(&anchored) {
                Ok(g) => {
                    builder.add(g);
                }
                Err(err) => {
                    tracing::warn!(
                        target: "mesh::properties",
                        "Ignoring invalid property_files pattern {raw:?}: {err}"
                    );
                }
            }
        }
        let set = builder.build().unwrap_or_else(|err| {
            tracing::error!(
                target: "mesh::properties",
                "property_files glob set failed to compile: {err}"
            );
            GlobSet::empty()
        });
        Self { set }
    }

    #[inline]
    pub fn is_match(&self, path: &Path) -> bool {
        self.set.is_match(path)
    }
}

/// PropertyRegistry for configuration flattening, placeholder resolution, and active secret masking.
///
/// Each key remembers the file it was last set from (`sources`), so an incremental reload can
/// call [`PropertyRegistry::remove_file`] to drop exactly the keys a deleted/edited file
/// contributed — mirroring `DocIndex::remove_file` and `ContractGraph::patch_files`.
///
/// **Shadowing decision**: if two files define the same key, the later-merged one wins (as
/// before). Removing the file that currently owns a shadowed key *drops* the key rather than
/// reviving the other file's value — the registry does not retain shadowed history. This matches
/// the precedent set by `DocIndex::remove_file` (which drops a file's sections outright, with no
/// attempt to recover anything superseded) and keeps the store a simple last-writer-wins map
/// instead of a per-key stack. A file that is still present and unchanged is not re-scanned on an
/// incremental reload, so it cannot "win back" a key it lost to a file that just got deleted; a
/// full rebuild (or touching the surviving file) re-establishes it.
#[derive(Debug, Clone)]
pub struct PropertyRegistry {
    flat_properties: HashMap<CompactStr, CompactStr>,
    sources: HashMap<CompactStr, PathBuf>,
    redact_secrets: bool,
}

impl Default for PropertyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PropertyRegistry {
    pub const SECRET_PATTERNS: &'static [&'static str] = &[
        "password",
        "secret",
        "token",
        "credential",
        "passphrase",
        "private",
        "jwt",
        "cert",
        "apikey",
        "api_key",
        "api-key",
        "secret_key",
        "access_key",
        "signing_key",
        "auth_token",
    ];

    pub const REDACTED_PLACEHOLDER: &'static str = "[REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]";

    pub fn new() -> Self {
        Self {
            flat_properties: HashMap::new(),
            sources: HashMap::new(),
            redact_secrets: true,
        }
    }

    /// Like [`Self::new`], but with `[engines.contracts.spring].auto_redact_secrets` wired
    /// through: when `false`, `insert_sanitized` (and the placeholder-default fallback) stop
    /// masking values that match [`Self::SECRET_PATTERNS`].
    pub fn with_redaction(redact_secrets: bool) -> Self {
        Self {
            redact_secrets,
            ..Self::new()
        }
    }

    /// Absorbs another registry (later keys win), recording `source` as the owning file for
    /// every key it contributed — lets per-file registries be built in parallel and folded into
    /// one while still supporting [`Self::remove_file`].
    pub fn merge(&mut self, other: PropertyRegistry, source: &Path) {
        for key in other.flat_properties.keys() {
            self.sources.insert(key.clone(), source.to_path_buf());
        }
        self.flat_properties.extend(other.flat_properties);
    }

    /// Drops every key currently attributed to `path` (used before re-indexing a changed file,
    /// and to clean up a deleted one). See the type-level doc for the shadowing decision.
    pub fn remove_file(&mut self, path: &Path) {
        let stale: Vec<CompactStr> = self
            .sources
            .iter()
            .filter(|(_, p)| p.as_path() == path)
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            self.flat_properties.remove(&key);
            self.sources.remove(&key);
        }
    }

    /// Resolves every `${key}` / `${key:default}` placeholder value in the registry in place,
    /// using the registry's own keys for lookups. Wired to
    /// `[engines.contracts.spring].resolve_placeholders`; callers skip this when the flag is
    /// off, leaving raw `${...}` values in the map. Single pass — a value that itself resolves
    /// to another placeholder is not re-resolved.
    pub fn resolve_all_placeholders(&mut self) {
        let updates: Vec<(CompactStr, CompactStr)> = self
            .flat_properties
            .iter()
            .filter_map(|(k, v)| {
                let resolved = self.resolve_placeholder(v.as_str());
                if resolved.as_ref() != v.as_str() {
                    Some((k.clone(), CompactStr::new(resolved.as_ref())))
                } else {
                    None
                }
            })
            .collect();
        for (key, value) in updates {
            self.flat_properties.insert(key, value);
        }
    }

    #[inline]
    pub fn redacted_count(&self) -> usize {
        self.flat_properties
            .values()
            .filter(|v| v.as_str() == Self::REDACTED_PLACEHOLDER)
            .count()
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

    /// Canonical, order-independent form of the registry (one sorted JSON line
    /// per key: key, value, owning file), used to compare two builds.
    pub fn canonical_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .flat_properties
            .iter()
            .map(|(key, value)| {
                let source = self
                    .sources
                    .get(key)
                    .map(|p| p.to_string_lossy().into_owned());
                serde_json::json!([key, value, source]).to_string()
            })
            .collect();
        lines.sort_unstable();
        lines
    }

    /// Word-exact "auth"/"authorization" tokens — deliberately NOT a bare
    /// substring pattern in `SECRET_PATTERNS` (a plain `.contains("auth")`
    /// false-positives on `app.author.email`, "author" containing "auth" as
    /// a substring but being an unrelated word). Checked against
    /// word-segment-split tokens instead, so `AUTH_KEY`/`api.authKey`/
    /// `Authorization` are still caught without `author`/`authoring`/etc.
    /// being swept in too.
    const AUTH_WORDS: &'static [&'static str] = &["auth", "authorization", "authn"];

    #[inline]
    pub fn is_sensitive_key(&self, key: &str) -> bool {
        if !self.redact_secrets {
            return false;
        }
        let lower = key.to_lowercase();
        if Self::SECRET_PATTERNS.iter().any(|&p| lower.contains(p)) {
            return true;
        }
        if (lower.ends_with(".key")
            || lower.ends_with("_key")
            || lower.ends_with("-key")
            || lower == "key")
            && (lower.contains("secret")
                || lower.contains("priv")
                || lower.contains("sign")
                || lower.contains("encrypt")
                || lower.contains("access")
                || lower.contains("token")
                || lower.contains("api")
                || lower.contains("cert"))
        {
            return true;
        }
        if Self::split_words(key)
            .iter()
            .any(|w| Self::AUTH_WORDS.contains(&w.as_str()))
        {
            return true;
        }
        false
    }

    /// Splits `s` into lowercase word segments on `.`/`_`/`-` and camelCase
    /// boundaries (lowercase-to-uppercase transitions), so a word-exact
    /// check doesn't false-positive on a substring occurring inside an
    /// unrelated word (e.g. "auth" inside "author").
    fn split_words(s: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut current = String::new();
        let mut prev_lower = false;
        for c in s.chars() {
            if c == '.' || c == '_' || c == '-' {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current).to_lowercase());
                }
                prev_lower = false;
                continue;
            }
            if c.is_uppercase() && prev_lower && !current.is_empty() {
                words.push(std::mem::take(&mut current).to_lowercase());
            }
            current.push(c);
            prev_lower = c.is_lowercase();
        }
        if !current.is_empty() {
            words.push(current.to_lowercase());
        }
        words
    }

    pub fn insert_sanitized(&mut self, key: &str, raw_val: &str) {
        let is_sensitive = self.is_sensitive_key(key);

        let sanitized_value = if is_sensitive {
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
            if self.is_sensitive_key(key) {
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

    /// `auto_redact_secrets = false` must produce observably different behaviour: the raw
    /// secret value survives instead of being masked.
    #[test]
    fn auto_redact_secrets_false_disables_masking() {
        let mut registry = PropertyRegistry::with_redaction(false);
        registry.insert_sanitized("spring.datasource.password", "super_secret_db_password");

        assert_eq!(
            registry.get("spring.datasource.password"),
            Some("super_secret_db_password")
        );
        assert_eq!(registry.redacted_count(), 0);

        // Default fallback in a placeholder must also stay unredacted.
        assert_eq!(
            registry.resolve_placeholder("${app.api_key:plain_default}"),
            "plain_default"
        );
    }

    /// `resolve_placeholders = true` must produce observably different behaviour vs. leaving it
    /// off: raw `${...}` values in the map get resolved in place.
    #[test]
    fn resolve_all_placeholders_rewrites_values_in_place() {
        let mut registry = PropertyRegistry::new();
        registry.insert_sanitized("db.host", "localhost");
        registry.insert_sanitized("app.url", "${db.host:127.0.0.1}");

        // Off (default: nothing calls resolve_all_placeholders): raw placeholder persists.
        assert_eq!(registry.get("app.url"), Some("${db.host:127.0.0.1}"));

        // On: the placeholder is rewritten in place.
        registry.resolve_all_placeholders();
        assert_eq!(registry.get("app.url"), Some("localhost"));
    }

    /// Per-key provenance: `remove_file` drops only the keys owned by that path, leaving
    /// keys contributed by other files untouched.
    #[test]
    fn remove_file_drops_only_that_files_keys() {
        let mut registry = PropertyRegistry::new();
        let mut a = PropertyRegistry::new();
        a.insert_sanitized("app.a.name", "a-value");
        let mut b = PropertyRegistry::new();
        b.insert_sanitized("app.b.name", "b-value");

        registry.merge(a, Path::new("/repo/a.properties"));
        registry.merge(b, Path::new("/repo/b.properties"));
        assert_eq!(registry.get("app.a.name"), Some("a-value"));
        assert_eq!(registry.get("app.b.name"), Some("b-value"));

        registry.remove_file(Path::new("/repo/b.properties"));
        assert_eq!(registry.get("app.a.name"), Some("a-value"));
        assert_eq!(registry.get("app.b.name"), None);
    }

    #[test]
    fn property_source_matcher_scopes_files() {
        let matcher = PropertySourceMatcher::compile(&["application*.properties".to_string()]);
        assert!(matcher.is_match(Path::new("/repo/src/main/resources/application.properties")));
        assert!(!matcher.is_match(Path::new("/repo/src/main/resources/other.properties")));
    }

    #[test]
    fn test_secret_redaction_boundary_preserves_innocent_keys() {
        let mut registry = PropertyRegistry::new();
        registry.insert_sanitized("kafka.partition.key", "order-id-123");
        registry.insert_sanitized("cache.lookup.key", "user-456");
        registry.insert_sanitized("app.author.email", "dev@example.com");
        registry.insert_sanitized("api.auth_token", "secret-token-789");
        registry.insert_sanitized("db.secret_key", "secret-key-abc");

        assert_eq!(registry.get("kafka.partition.key"), Some("order-id-123"));
        assert_eq!(registry.get("cache.lookup.key"), Some("user-456"));
        assert_eq!(registry.get("app.author.email"), Some("dev@example.com"));
        assert_eq!(
            registry.get("api.auth_token"),
            Some(PropertyRegistry::REDACTED_PLACEHOLDER)
        );
        assert_eq!(
            registry.get("db.secret_key"),
            Some(PropertyRegistry::REDACTED_PLACEHOLDER)
        );
    }

    /// Regression test: dropping bare "auth" from `SECRET_PATTERNS` (to stop
    /// `app.author.email` false-positiving on "auth" being a substring of
    /// "author") must not also stop real auth credentials from being
    /// redacted — `AUTH_KEY`, `api.authKey`, and `Authorization` are all
    /// real secret-bearing keys.
    #[test]
    fn auth_keys_are_still_redacted_without_flagging_author() {
        let mut registry = PropertyRegistry::new();
        registry.insert_sanitized("AUTH_KEY", "super-secret-value");
        registry.insert_sanitized("api.authKey", "another-secret");
        registry.insert_sanitized("Authorization", "Bearer abc123");
        registry.insert_sanitized("app.author.email", "dev@example.com");

        assert_eq!(
            registry.get("AUTH_KEY"),
            Some(PropertyRegistry::REDACTED_PLACEHOLDER),
            "AUTH_KEY must still be redacted"
        );
        assert_eq!(
            registry.get("api.authKey"),
            Some(PropertyRegistry::REDACTED_PLACEHOLDER),
            "api.authKey must still be redacted"
        );
        assert_eq!(
            registry.get("Authorization"),
            Some(PropertyRegistry::REDACTED_PLACEHOLDER),
            "Authorization must still be redacted"
        );
        assert_eq!(
            registry.get("app.author.email"),
            Some("dev@example.com"),
            "app.author.email must NOT be redacted — 'author' is not 'auth'"
        );
    }
}
