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
    /// The literal value as ingested — before secret redaction and before
    /// `${...}` placeholder resolution. This is the raw fact; `flat_properties`
    /// is purely derived from it. Keeping the two separate is what lets
    /// `resolve_all_placeholders` be re-run after some *other* key it depends on
    /// changes (an incremental reload of a different file) and still produce the
    /// same result a full rebuild would: once a value is resolved in place, its
    /// own `${...}` template is gone, and there is nothing left to re-resolve
    /// against a new dependency value on a later call (idempotence invariants
    /// I2/I3).
    raw_values: HashMap<CompactStr, CompactStr>,
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
            raw_values: HashMap::new(),
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
        self.raw_values.extend(other.raw_values);
        self.flat_properties.extend(other.flat_properties);
    }

    /// Drops every key currently attributed to `path` (used before re-indexing a changed file,
    /// and to clean up a deleted one). See the type-level doc for the shadowing decision.
    pub fn remove_file(&mut self, path: &Path) {
        self.remove_files(&std::iter::once(path).collect());
    }

    /// [`Self::remove_file`] for a whole batch in one pass over `sources`. Called
    /// once per file, a reload of k changed files cost k full passes — O(k·N) on
    /// a mass change (branch switch) over a large registry.
    pub fn remove_files(&mut self, paths: &std::collections::HashSet<&Path>) {
        if paths.is_empty() {
            return;
        }
        let stale: Vec<CompactStr> = self
            .sources
            .iter()
            .filter(|(_, p)| paths.contains(p.as_path()))
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            self.flat_properties.remove(&key);
            self.raw_values.remove(&key);
            self.sources.remove(&key);
        }
    }

    /// Resolves every `${key}` / `${key:default}` placeholder value into `flat_properties`,
    /// derived fresh from `raw_values` every time rather than mutating the previous result in
    /// place — the latter would, once a key resolved once, permanently discard its own
    /// `${...}` template and make it impossible to notice a *different* key it depends on
    /// changing on a later incremental reload (see the type-level doc). Wired to
    /// `[engines.contracts.spring].resolve_placeholders`; callers skip this when the flag is
    /// off, leaving raw `${...}` values in the map. Single pass — a value that itself resolves
    /// to another placeholder is not re-resolved.
    pub fn resolve_all_placeholders(&mut self) {
        let keys: Vec<CompactStr> = self.raw_values.keys().cloned().collect();
        for key in keys {
            // Redaction already happened correctly at ingestion time
            // (`insert_sanitized`, using that scan's own `auto_redact_secrets`
            // setting) — never peek at a sensitive key's raw value here, or a
            // secret redacted once could leak back out on a later reload.
            if self.flat_properties.get(&key).map(CompactStr::as_str)
                == Some(Self::REDACTED_PLACEHOLDER)
            {
                continue;
            }
            let Some(raw) = self.raw_values.get(&key).cloned() else {
                continue;
            };
            let resolved = self.resolve_placeholder(raw.as_str()).into_owned();
            self.flat_properties.insert(key, CompactStr::new(resolved));
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

        self.raw_values
            .insert(CompactStr::new(key), CompactStr::new(raw_val));
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

    /// Ingests YAML configuration and flattens nested keys with dot notation.
    ///
    /// Streams the first YAML document's events straight into flat
    /// `(dotted.key, value)` pairs through a serde visitor — no
    /// `serde_yaml::Value` tree is ever built (`serde_yaml` still buffers the
    /// document's event list; see `yaml_flatten`). Only the first document is read:
    /// in a multi-document Spring file (`---` profile sections) it is the default
    /// profile, and merging later profile documents over it would report
    /// profile-specific overrides as the base value. (`serde_yaml::from_str`,
    /// used before, rejected multi-document files outright, ingesting nothing.)
    /// The pairs are only inserted once the document parsed cleanly, so a
    /// malformed file still contributes nothing rather than a partial prefix.
    pub fn ingest_yaml_str(&mut self, content: &str) -> Result<(), serde_yaml::Error> {
        let Some(document) = serde_yaml::Deserializer::from_str(content).next() else {
            return Ok(());
        };
        let mut pairs: Vec<(String, String)> = Vec::new();
        serde::de::DeserializeSeed::deserialize(
            yaml_flatten::FlattenSeed {
                prefix: String::new(),
                out: &mut pairs,
            },
            document,
        )?;
        for (key, value) in pairs {
            self.insert_sanitized(&key, &value);
        }
        Ok(())
    }
}

/// Streaming YAML → flat dotted-key pairs. Mirrors the old `Value`-tree
/// flattening: string keys only (a non-string key skips its subtree; a tagged
/// string key is read through its tag, as `Value::as_str` did), strings/numbers/
/// bools become values (numbers formatted through `serde_yaml::Number`, as
/// `Value`'s `Display` did), sequences, nulls and tagged values are skipped, and
/// a mapping with a repeated scalar key fails the document as `Value`'s
/// `Mapping` did. The one intended difference is multi-document input (see
/// `ingest_yaml_str`).
///
/// "Streaming" is relative to the `Value` tree only: `serde_yaml` 0.9 still
/// loads the document's whole event list before any visitor runs, so peak
/// memory is that event list plus the flat pairs. What bounds it is the
/// indexer's per-file size budget (`AstGuard::within_size_budget`), not this
/// visitor.
mod yaml_flatten {
    use serde::de::{
        self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor,
    };
    use std::collections::HashSet;
    use std::fmt;

    pub(super) struct FlattenSeed<'a> {
        pub(super) prefix: String,
        pub(super) out: &'a mut Vec<(String, String)>,
    }

    impl<'de> DeserializeSeed<'de> for FlattenSeed<'_> {
        type Value = ();

        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            deserializer.deserialize_any(self)
        }
    }

    impl FlattenSeed<'_> {
        fn scalar(self, value: String) {
            self.out.push((self.prefix, value));
        }
    }

    impl<'de> Visitor<'de> for FlattenSeed<'_> {
        type Value = ();

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a YAML value")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<(), E> {
            self.scalar(v.to_string());
            Ok(())
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<(), E> {
            self.scalar(v);
            Ok(())
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<(), E> {
            self.scalar(v.to_string());
            Ok(())
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<(), E> {
            self.scalar(serde_yaml::Number::from(v).to_string());
            Ok(())
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<(), E> {
            self.scalar(serde_yaml::Number::from(v).to_string());
            Ok(())
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<(), E> {
            self.scalar(serde_yaml::Number::from(v).to_string());
            Ok(())
        }

        fn visit_unit<E: de::Error>(self) -> Result<(), E> {
            Ok(())
        }

        fn visit_none<E: de::Error>(self) -> Result<(), E> {
            Ok(())
        }

        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(self)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(())
        }

        fn visit_enum<A: de::EnumAccess<'de>>(self, data: A) -> Result<(), A::Error> {
            // A tagged value (`!Tag value`); the tree flattening skipped these.
            let (IgnoredAny, variant) = data.variant::<IgnoredAny>()?;
            de::VariantAccess::newtype_variant::<IgnoredAny>(variant)?;
            Ok(())
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            // `serde_yaml::Value` rejected a mapping with a repeated key, so the
            // whole file ingested nothing (Spring's own YAML loader refuses it
            // too). Kept: with no check here a repeated key would silently ingest
            // both values, the later one winning.
            let mut seen: HashSet<KeyId> = HashSet::new();
            while let Some(key) = map.next_key::<StrKey>()? {
                if let Some(id) = key.1 {
                    if !seen.insert(id) {
                        return Err(de::Error::custom("duplicate entry in a YAML mapping"));
                    }
                }
                match key.0 {
                    Some(k) => {
                        let prefix = if self.prefix.is_empty() {
                            k
                        } else {
                            format!("{}.{k}", self.prefix)
                        };
                        map.next_value_seed(FlattenSeed {
                            prefix,
                            out: &mut *self.out,
                        })?;
                    }
                    None => {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
            }
            Ok(())
        }
    }

    /// Identity of a scalar mapping key, for duplicate detection. Mirrors
    /// `serde_yaml::Value` equality for the scalar shapes (a tagged key differs
    /// from its untagged content and from the same content under another tag);
    /// collection keys are not tracked.
    #[derive(PartialEq, Eq, Hash)]
    enum KeyId {
        Str(String),
        Bool(bool),
        Int(i128),
        Float(u64),
        Null,
        Tagged(String, Box<KeyId>),
    }

    /// A mapping key: `.0` is `Some` for a string key (a tagged string key is
    /// read through its tag, as `Value::as_str` did), `None` for any other
    /// shape; `.1` is its identity when it is a scalar.
    struct StrKey(Option<String>, Option<KeyId>);

    impl<'de> de::Deserialize<'de> for StrKey {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct KeyVisitor;
            impl<'de> Visitor<'de> for KeyVisitor {
                type Value = StrKey;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a mapping key")
                }
                fn visit_str<E: de::Error>(self, v: &str) -> Result<StrKey, E> {
                    Ok(StrKey(Some(v.to_string()), Some(KeyId::Str(v.to_string()))))
                }
                fn visit_string<E: de::Error>(self, v: String) -> Result<StrKey, E> {
                    let id = KeyId::Str(v.clone());
                    Ok(StrKey(Some(v), Some(id)))
                }
                fn visit_bool<E: de::Error>(self, v: bool) -> Result<StrKey, E> {
                    Ok(StrKey(None, Some(KeyId::Bool(v))))
                }
                fn visit_i64<E: de::Error>(self, v: i64) -> Result<StrKey, E> {
                    Ok(StrKey(None, Some(KeyId::Int(i128::from(v)))))
                }
                fn visit_u64<E: de::Error>(self, v: u64) -> Result<StrKey, E> {
                    Ok(StrKey(None, Some(KeyId::Int(i128::from(v)))))
                }
                fn visit_f64<E: de::Error>(self, v: f64) -> Result<StrKey, E> {
                    Ok(StrKey(None, Some(KeyId::Float(v.to_bits()))))
                }
                fn visit_unit<E: de::Error>(self) -> Result<StrKey, E> {
                    Ok(StrKey(None, Some(KeyId::Null)))
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<StrKey, A::Error> {
                    while seq.next_element::<IgnoredAny>()?.is_some() {}
                    Ok(StrKey(None, None))
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<StrKey, A::Error> {
                    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                    Ok(StrKey(None, None))
                }
                /// A tagged key (`!Tag key: v`): `Value::as_str` saw through the
                /// tag, so the key still names its subtree.
                fn visit_enum<A: de::EnumAccess<'de>>(self, data: A) -> Result<StrKey, A::Error> {
                    let (tag, variant) = data.variant::<String>()?;
                    let StrKey(key, id) = de::VariantAccess::newtype_variant::<StrKey>(variant)?;
                    Ok(StrKey(key, id.map(|id| KeyId::Tagged(tag, Box::new(id)))))
                }
            }
            d.deserialize_any(KeyVisitor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-streaming implementation, kept verbatim as the oracle.
    fn dom_flatten(prefix: &str, value: &serde_yaml::Value, out: &mut Vec<(String, String)>) {
        match value {
            serde_yaml::Value::Mapping(map) => {
                for (k, v) in map {
                    if let Some(k_str) = k.as_str() {
                        let new_prefix = if prefix.is_empty() {
                            k_str.to_string()
                        } else {
                            format!("{prefix}.{k_str}")
                        };
                        dom_flatten(&new_prefix, v, out);
                    }
                }
            }
            serde_yaml::Value::String(s) => out.push((prefix.to_string(), s.clone())),
            serde_yaml::Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
            serde_yaml::Value::Bool(b) => out.push((prefix.to_string(), b.to_string())),
            _ => {}
        }
    }

    /// Streaming flattening yields exactly the pairs the `Value`-tree
    /// flattening did, across every scalar/collection shape.
    #[test]
    fn streaming_yaml_matches_dom_flattening() {
        let yaml = r#"
server:
  port: 8080
  ratio: 1.0
  big: 18446744073709551615
  neg: -3
  inf: .inf
  enabled: true
  name: "billing"
  empty: ~
  list: [a, b, {nested: x}]
  1: numeric-key-skipped
  ? [complex, key]
  : skipped
  tagged: !Custom value
anchors:
  base: &b
    url: http://x
  copy: *b
"#;
        let mut streamed = Vec::new();
        let doc = serde_yaml::Deserializer::from_str(yaml)
            .next()
            .expect("doc");
        serde::de::DeserializeSeed::deserialize(
            yaml_flatten::FlattenSeed {
                prefix: String::new(),
                out: &mut streamed,
            },
            doc,
        )
        .expect("stream");
        let dom: serde_yaml::Value = serde_yaml::from_str(yaml).expect("dom");
        let mut expected = Vec::new();
        dom_flatten("", &dom, &mut expected);
        assert_eq!(streamed, expected);
        assert!(streamed
            .iter()
            .any(|(k, v)| k == "server.ratio" && v == "1.0"));
        assert!(streamed
            .iter()
            .any(|(k, v)| k == "anchors.copy.url" && v == "http://x"));
    }

    /// A multi-document Spring file used to be rejected wholesale; now its first
    /// (default-profile) document is ingested and later profiles don't override it.
    #[test]
    fn multi_document_yaml_ingests_first_document_only() {
        let mut reg = PropertyRegistry::new();
        reg.ingest_yaml_str("app:\n  mode: default\n---\napp:\n  mode: prod\n")
            .expect("multi-doc");
        assert_eq!(reg.get("app.mode"), Some("default"));
    }

    /// Shapes where the streaming visitor used to diverge from the `Value`
    /// tree: a tagged key (read through its tag) and a repeated key (whole
    /// file rejected). Each is checked against the tree itself.
    #[test]
    fn streaming_yaml_matches_dom_on_tagged_and_duplicate_keys() {
        let cases = [
            "!foo k: v\nb: 1\n",
            "? !foo k\n: {x: 1}\nb: 1\n",
            "! k: v\nb: 1\n",
            "a: 1\na: 2\n",
            "a:\n  b: 1\n  b: 2\n",
            "1: a\n1: b\nc: 3\n",
            "true: a\ntrue: b\n",
            "~: a\n~: b\n",
            "!a k: 1\n!b k: 2\n",
            "!a k: 1\nk: 2\n",
            "1: a\n'1': b\n",
            "a: 1\nb: {a: 2}\n",
        ];
        for yaml in cases {
            let dom = serde_yaml::from_str::<serde_yaml::Value>(yaml).map(|v| {
                let mut out = Vec::new();
                dom_flatten("", &v, &mut out);
                out
            });
            let mut streamed = Vec::new();
            let doc = serde_yaml::Deserializer::from_str(yaml)
                .next()
                .expect("doc");
            let result = serde::de::DeserializeSeed::deserialize(
                yaml_flatten::FlattenSeed {
                    prefix: String::new(),
                    out: &mut streamed,
                },
                doc,
            );
            match dom {
                Ok(expected) => {
                    assert!(result.is_ok(), "{yaml:?}: {result:?}");
                    assert_eq!(streamed, expected, "{yaml:?}");
                }
                Err(_) => assert!(result.is_err(), "{yaml:?} must be rejected"),
            }
        }
        let mut reg = PropertyRegistry::new();
        assert!(reg.ingest_yaml_str("a: 1\nb: 2\na: 3\n").is_err());
        assert_eq!(reg.get("b"), None, "a rejected file ingests nothing");
    }

    /// Malformed YAML contributes nothing, not a partial prefix.
    #[test]
    fn malformed_yaml_is_atomic() {
        let mut reg = PropertyRegistry::new();
        assert!(reg.ingest_yaml_str("a: 1\nb: [unclosed\n").is_err());
        assert_eq!(reg.get("a"), None);
    }

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
    fn remove_files_drops_a_whole_batch() {
        let mut registry = PropertyRegistry::new();
        for f in ["a", "b", "c"] {
            let mut r = PropertyRegistry::new();
            r.insert_sanitized(&format!("app.{f}"), f);
            registry.merge(r, Path::new(&format!("/repo/{f}.properties")));
        }
        registry.remove_files(
            &[
                Path::new("/repo/a.properties"),
                Path::new("/repo/c.properties"),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(registry.get("app.a"), None);
        assert_eq!(registry.get("app.b"), Some("b"));
        assert_eq!(registry.get("app.c"), None);
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
