//! What a config edit costs `meshd` to apply.
//!
//! Most config keys never touch what was indexed: a route prefix, a skill
//! recommendation, a stop rule or a docs alias is read at query time, so the
//! running index stays valid and only the config (and the state built from it)
//! is swapped. A changed custom pattern changes the facts of the files it
//! applies to, and nothing else. Everything else (roots, excludes, proto
//! directories, engine switches, ...) can move any file's facts and is a full
//! rebuild, which still reuses the persistent per-file parse cache.

use crate::config::{Config, CustomPatternConfig};
use serde_json::Value;
use std::path::PathBuf;

/// How a new config is applied to the serving state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadKind {
    /// Only keys read at query time changed: swap the config, keep the index.
    DisplayOnly,
    /// Only custom patterns changed: re-extract the files they apply to.
    Partial,
    /// Anything that can move any file's facts: rebuild the index.
    Full,
}

impl ReloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DisplayOnly => "display-only",
            Self::Partial => "partial",
            Self::Full => "full",
        }
    }
}

/// The kind of a reload, plus the patterns whose files a `Partial` one must
/// re-extract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadPlan {
    pub kind: ReloadKind,
    /// Patterns added, removed or edited (all of them when only their order
    /// changed). Empty unless `kind` is [`ReloadKind::Partial`].
    pub changed_patterns: Vec<CustomPatternConfig>,
}

impl ReloadPlan {
    fn of(kind: ReloadKind) -> Self {
        Self {
            kind,
            changed_patterns: Vec::new(),
        }
    }

    /// Whether `path` (a file path string) is governed by a changed pattern, so
    /// its facts must be extracted again.
    pub fn forces(&self, path: &str) -> bool {
        self.changed_patterns.iter().any(|p| p.applies_to(path))
    }
}

/// The part of `config` that decides what gets indexed, as a comparable value.
///
/// Fails closed: every config struct is destructured with every field named and
/// no `..`, so adding a field to any of them is a compile error here until it is
/// classified. A field is listed in the returned value (it can move what gets
/// indexed: a change is a full rebuild), bound to `_` with a reason (read at
/// query time or only displayed: a change swaps the config), or, for custom
/// patterns, handled by [`changed_patterns`] (a change is partial).
fn index_inputs(config: &Config) -> Option<Value> {
    use crate::config::{
        CacheConfig, ContractsConfig, DocsConfig, EnginesConfig, PolicyConfig, WorkspaceConfig,
    };
    let Config {
        workspace,
        engines,
        cache,
    } = config;
    let WorkspaceConfig {
        // Display only.
        name: _,
        version: _,
        // Route prefixes are applied when a route is displayed or matched.
        http_prefix: _,
        http_prefixes: _,
        workspace_root,
        roots,
        exclude_patterns,
        mount_aliases,
        // Derived from `workspace_root`, compared by `classify`.
        resolved_workspace_root: _,
    } = workspace;
    let EnginesConfig {
        docs,
        contracts,
        policy,
    } = engines;
    // An absent section means the section with its defaults.
    let docs: DocsConfig = match docs {
        Some(d) => d.clone(),
        None => serde_json::from_value(serde_json::json!({})).ok()?,
    };
    let contracts: ContractsConfig = match contracts {
        Some(c) => c.clone(),
        None => serde_json::from_value(serde_json::json!({})).ok()?,
    };
    let DocsConfig {
        enabled: docs_enabled,
        paths: docs_paths,
        // Query time: ranking and expansion settings read by every search.
        aliases: _,
        stop_words: _,
        exact_phrase_boost: _,
        fuzzy_fallback: _,
        // Applied while a markdown file is parsed: moves what is indexed.
        sanitize_prompt_injections,
    } = docs;
    let ContractsConfig {
        enabled: contracts_enabled,
        grpc,
        spring,
        openapi,
        asyncapi,
        cpp,
        // Partial reload, see `changed_patterns`.
        patterns: _,
    } = contracts;
    // Nothing in the policy engine reaches the index. `AppState::new` rebuilds the
    // governance engine (stop rules, skills, `read_governance_mode`, `enabled`)
    // from the new config, `cryptographic_audit_trail` is read from the serving
    // state on every audit write, and `enforce_git_hooks` only matters to
    // `mesh-mcp install-hooks`. When in doubt about a new key, bind it above, not here.
    let PolicyConfig {
        enabled: _,
        enforce_git_hooks: _,
        cryptographic_audit_trail: _,
        read_governance_mode: _,
        stop_rules: _,
        skills: _,
    } = policy.clone().unwrap_or_default();
    let CacheConfig { max_size_mb } = cache;
    serde_json::to_value((
        workspace_root,
        roots,
        exclude_patterns,
        mount_aliases,
        docs_enabled,
        docs_paths,
        sanitize_prompt_injections,
        contracts_enabled,
        grpc,
        spring,
        openapi,
        asyncapi,
        cpp,
        max_size_mb,
    ))
    .ok()
}

fn patterns(config: &Config) -> &[CustomPatternConfig] {
    config
        .engines
        .contracts
        .as_ref()
        .map_or(&[], |c| c.patterns.as_slice())
}

/// Patterns present on one side only; when the multisets are equal but the order
/// differs, every pattern of `new` (order decides how their facts merge).
fn changed_patterns(
    old: &[CustomPatternConfig],
    new: &[CustomPatternConfig],
) -> Vec<CustomPatternConfig> {
    if old == new {
        return Vec::new();
    }
    let mut left: Vec<&CustomPatternConfig> = old.iter().collect();
    let mut added: Vec<CustomPatternConfig> = Vec::new();
    for p in new {
        match left.iter().position(|o| *o == p) {
            Some(i) => {
                left.remove(i);
            }
            None => added.push(p.clone()),
        }
    }
    let mut changed: Vec<CustomPatternConfig> = left.into_iter().cloned().collect();
    changed.extend(added);
    if changed.is_empty() {
        new.to_vec()
    } else {
        changed
    }
}

/// Classifies the change from `old` (built over `old_roots`) to `new` (over
/// `new_roots`). Conservative: anything it cannot prove query-time or
/// pattern-only is [`ReloadKind::Full`].
pub fn classify(
    old: &Config,
    old_roots: &[PathBuf],
    new: &Config,
    new_roots: &[PathBuf],
) -> ReloadPlan {
    if old_roots != new_roots
        || old.workspace.resolved_workspace_root != new.workspace.resolved_workspace_root
    {
        return ReloadPlan::of(ReloadKind::Full);
    }
    let (Some(a), Some(b)) = (index_inputs(old), index_inputs(new)) else {
        return ReloadPlan::of(ReloadKind::Full);
    };
    if a != b {
        return ReloadPlan::of(ReloadKind::Full);
    }
    let changed = changed_patterns(patterns(old), patterns(new));
    if changed.is_empty() {
        ReloadPlan::of(ReloadKind::DisplayOnly)
    } else {
        ReloadPlan {
            kind: ReloadKind::Partial,
            changed_patterns: changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "[workspace]\nname = \"w\"\nroots = [\"a\"]\nhttp_prefix = \"/api\"\n\
        [engines.contracts]\n\
        [[engines.contracts.patterns]]\nname = \"p1\"\nkind = \"topic_producer\"\nregex = 'x(.)'\nfile_pattern = \"*.ts\"\n\
        [engines.docs]\naliases = { a = \"b\" }\n\
        [engines.policy]\nenabled = true\n";

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/w/a")]
    }

    fn plan(old: &str, new: &str) -> ReloadPlan {
        let o = Config::load_from_str(old).expect("old");
        let n = Config::load_from_str(new).expect("new");
        classify(&o, &roots(), &n, &roots())
    }

    #[test]
    fn identical_configs_swap_only() {
        assert_eq!(plan(BASE, BASE).kind, ReloadKind::DisplayOnly);
    }

    #[test]
    fn display_and_policy_keys_never_reindex() {
        for new in [
            BASE.replace("/api", "/api/v2"),
            BASE.replace("name = \"w\"", "name = \"renamed\""),
            BASE.replace("enabled = true", "enabled = false"),
            BASE.replace("a = \"b\"", "a = \"c\""),
            format!("{BASE}[workspace.http_prefixes]\n\"a\" = \"/x\"\n"),
        ] {
            let p = plan(BASE, &new);
            assert_eq!(p.kind, ReloadKind::DisplayOnly, "{new}");
        }
        let with_skills = format!("{BASE}[engines.policy.skills]\n\"k\" = \"s.md\"\n");
        assert_eq!(plan(BASE, &with_skills).kind, ReloadKind::DisplayOnly);
        let stop = with_skills.replace("[engines.policy.skills]", "[engines.policy.stop_rules]");
        assert_eq!(plan(&with_skills, &stop).kind, ReloadKind::DisplayOnly);
    }

    #[test]
    fn a_pattern_edit_is_partial_and_scoped_by_file_pattern() {
        let edited = BASE.replace("x(.)", "y(.)");
        let p = plan(BASE, &edited);
        assert_eq!(p.kind, ReloadKind::Partial);
        // The old and the new version of the edited pattern both count.
        assert_eq!(p.changed_patterns.len(), 2);
        assert!(p.forces("/w/a/src/x.ts"));
        assert!(!p.forces("/w/a/src/x.py"));

        let added = format!(
            "{BASE}[[engines.contracts.patterns]]\nname = \"p2\"\nkind = \"saga\"\nregex = 'z'\nfile_pattern = \"*.java\"\n"
        );
        let p = plan(BASE, &added);
        assert_eq!(p.kind, ReloadKind::Partial);
        assert_eq!(p.changed_patterns.len(), 1);
        assert!(p.forces("/w/a/B.java"));
        assert!(!p.forces("/w/a/b.ts"));

        let unscoped = BASE.replace("file_pattern = \"*.ts\"\n", "");
        let p = plan(BASE, &unscoped);
        assert!(p.forces("/w/a/anything.rs"));
    }

    #[test]
    fn anything_that_can_move_facts_is_full() {
        let o = Config::load_from_str(BASE).expect("old");
        let two_roots = vec![PathBuf::from("/w/a"), PathBuf::from("/w/b")];
        let n = Config::load_from_str(&BASE.replace("[\"a\"]", "[\"a\", \"b\"]")).expect("new");
        assert_eq!(
            classify(&o, &roots(), &n, &two_roots).kind,
            ReloadKind::Full
        );
        for new in [
            format!("{BASE}[engines.contracts.grpc]\nproto_dirs = [\"protos\"]\n"),
            BASE.replace("[engines.docs]\n", "[engines.docs]\nenabled = false\n"),
            BASE.replace(
                "[engines.docs]\n",
                "[engines.docs]\npaths = [\"docs/**\"]\n",
            ),
            BASE.replace(
                "[engines.docs]\n",
                "[engines.docs]\nsanitize_prompt_injections = false\n",
            ),
            BASE.replace(
                "name = \"w\"\n",
                "name = \"w\"\nexclude_patterns = [\"**/x/**\"]\n",
            ),
            BASE.replace(
                "[engines.contracts]\n",
                "[engines.contracts]\nenabled = false\n",
            ),
        ] {
            let p = plan(BASE, &new);
            assert_eq!(p.kind, ReloadKind::Full, "{new}");
        }
    }

    #[test]
    fn a_pure_reorder_of_patterns_is_partial() {
        let two = format!(
            "{BASE}[[engines.contracts.patterns]]\nname = \"p2\"\nkind = \"saga\"\nregex = 'z'\n"
        );
        let swapped = two
            .replace("name = \"p1\"", "name = \"TMP\"")
            .replace("name = \"p2\"", "name = \"p1\"")
            .replace("name = \"TMP\"", "name = \"p2\"");
        // Renaming is an edit, not a reorder: still partial, both sides listed.
        assert_eq!(plan(&two, &swapped).kind, ReloadKind::Partial);
    }

    /// Every key the config can serialize, with the kind of reload its change
    /// must cause. The compile-time guard is the exhaustive destructuring in
    /// `index_inputs`; this table is the behavioural one: a key missing from it
    /// (a field added without being classified here) fails the test, and each
    /// key is changed in turn and must classify as listed.
    const KEY_KINDS: &[(&str, ReloadKind)] = &[
        ("workspace.name", ReloadKind::DisplayOnly),
        ("workspace.version", ReloadKind::DisplayOnly),
        ("workspace.http_prefix", ReloadKind::DisplayOnly),
        ("workspace.http_prefixes", ReloadKind::DisplayOnly),
        ("workspace.workspace_root", ReloadKind::Full),
        ("workspace.roots", ReloadKind::Full),
        ("workspace.exclude_patterns", ReloadKind::Full),
        ("workspace.mount_aliases", ReloadKind::Full),
        ("cache.max_size_mb", ReloadKind::Full),
        ("engines.docs.enabled", ReloadKind::Full),
        ("engines.docs.paths", ReloadKind::Full),
        ("engines.docs.aliases", ReloadKind::DisplayOnly),
        ("engines.docs.stop_words", ReloadKind::DisplayOnly),
        ("engines.docs.exact_phrase_boost", ReloadKind::DisplayOnly),
        ("engines.docs.fuzzy_fallback", ReloadKind::DisplayOnly),
        ("engines.docs.sanitize_prompt_injections", ReloadKind::Full),
        ("engines.contracts.enabled", ReloadKind::Full),
        ("engines.contracts.patterns", ReloadKind::Partial),
        ("engines.contracts.grpc.proto_dirs", ReloadKind::Full),
        (
            "engines.contracts.grpc.controller_annotations",
            ReloadKind::Full,
        ),
        (
            "engines.contracts.grpc.canonical_fqcn_projection",
            ReloadKind::Full,
        ),
        ("engines.contracts.spring.enabled", ReloadKind::Full),
        ("engines.contracts.spring.property_files", ReloadKind::Full),
        (
            "engines.contracts.spring.resolve_placeholders",
            ReloadKind::Full,
        ),
        (
            "engines.contracts.spring.auto_redact_secrets",
            ReloadKind::Full,
        ),
        ("engines.contracts.openapi.enabled", ReloadKind::Full),
        ("engines.contracts.openapi.spec_files", ReloadKind::Full),
        ("engines.contracts.asyncapi.enabled", ReloadKind::Full),
        ("engines.contracts.asyncapi.spec_files", ReloadKind::Full),
        (
            "engines.contracts.asyncapi.infer_string_topics",
            ReloadKind::Full,
        ),
        ("engines.contracts.cpp.include_paths", ReloadKind::Full),
        ("engines.policy.enabled", ReloadKind::DisplayOnly),
        ("engines.policy.enforce_git_hooks", ReloadKind::DisplayOnly),
        (
            "engines.policy.cryptographic_audit_trail",
            ReloadKind::DisplayOnly,
        ),
        (
            "engines.policy.read_governance_mode",
            ReloadKind::DisplayOnly,
        ),
        ("engines.policy.stop_rules", ReloadKind::DisplayOnly),
        ("engines.policy.skills", ReloadKind::DisplayOnly),
    ];

    /// Maps whose keys are chosen by the user: the map is one config key.
    const USER_KEYED: &[&str] = &[
        "workspace.mount_aliases",
        "workspace.http_prefixes",
        "engines.docs.aliases",
        "engines.policy.stop_rules",
        "engines.policy.skills",
    ];

    const FULL: &str = "[workspace]\nname = \"w\"\nversion = \"1\"\nworkspace_root = \".\"\n\
        roots = [\"a\"]\nexclude_patterns = [\"**/x/**\"]\nhttp_prefix = \"/api\"\n\
        [workspace.mount_aliases]\n\"/ws\" = \"a\"\n\
        [workspace.http_prefixes]\n\"a\" = \"/a\"\n\
        [cache]\nmax_size_mb = 10\n\
        [engines.docs]\nenabled = true\npaths = [\"docs/**\"]\nstop_words = [\"the\"]\n\
        exact_phrase_boost = 5\nfuzzy_fallback = false\nsanitize_prompt_injections = false\n\
        [engines.docs.aliases]\nk = \"v\"\n\
        [engines.contracts]\nenabled = true\n\
        [engines.contracts.grpc]\nproto_dirs = [\"p\"]\ncontroller_annotations = [\"@G\"]\ncanonical_fqcn_projection = false\n\
        [engines.contracts.spring]\nenabled = true\nproperty_files = [\"a.properties\"]\nresolve_placeholders = false\nauto_redact_secrets = false\n\
        [engines.contracts.openapi]\nenabled = true\nspec_files = [\"o.yaml\"]\n\
        [engines.contracts.asyncapi]\nenabled = true\nspec_files = [\"a.yaml\"]\ninfer_string_topics = false\n\
        [engines.contracts.cpp]\ninclude_paths = [\"inc\"]\n\
        [[engines.contracts.patterns]]\nname = \"p\"\nkind = \"saga\"\nregex = 'x'\nfile_pattern = \"*.ts\"\ntarget_group = 1\nconsumer_group = 2\nconfidence = \"exact\"\n\
        [engines.policy]\nenabled = true\nenforce_git_hooks = false\ncryptographic_audit_trail = false\n\
        read_governance_mode = \"audit_warn\"\n\
        [engines.policy.stop_rules]\nr = \"stop\"\n\
        [engines.policy.skills]\ns = \"s.md\"\n";

    fn key_paths(value: &Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) if !USER_KEYED.contains(&prefix) => {
                for (k, v) in map {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    key_paths(v, &path, out);
                }
            }
            _ => out.push(prefix.to_string()),
        }
    }

    fn mutate(value: &mut Value, path: &str) {
        match value {
            Value::String(s) => {
                *s = match s.as_str() {
                    "audit_warn" => "enforce_refusal".to_string(),
                    other => format!("{other}-changed"),
                }
            }
            Value::Bool(b) => *b = !*b,
            Value::Number(n) => {
                *value = Value::from(n.as_u64().unwrap_or(0) + 1);
            }
            Value::Array(items) => {
                if path == "engines.contracts.patterns" {
                    items.push(serde_json::json!({
                        "name": "added", "kind": "saga", "regex": "y",
                        "file_pattern": null, "target_group": 1,
                        "consumer_group": null, "confidence": null
                    }));
                } else {
                    items.push(Value::String("added".to_string()));
                }
            }
            Value::Object(map) => {
                map.insert("added".to_string(), Value::String("added".to_string()));
            }
            Value::Null => *value = Value::String("added".to_string()),
        }
    }

    fn at<'v>(root: &'v mut Value, path: &str) -> &'v mut Value {
        let mut cur = root;
        for part in path.split('.') {
            cur = cur.get_mut(part).expect("path exists");
        }
        cur
    }

    #[test]
    fn every_config_key_is_classified_and_classifies_as_listed() {
        let config = Config::load_from_str(FULL).expect("fully populated config");
        let value = serde_json::to_value(&config).expect("serialize");
        let mut keys = Vec::new();
        key_paths(&value, "", &mut keys);
        keys.sort();
        let mut listed: Vec<&str> = KEY_KINDS.iter().map(|(k, _)| *k).collect();
        listed.sort_unstable();
        assert_eq!(
            keys.iter().map(String::as_str).collect::<Vec<_>>(),
            listed,
            "a config key is missing from KEY_KINDS (or KEY_KINDS lists a key that no longer exists): classify it"
        );

        let roots = roots();
        for (path, expected) in KEY_KINDS {
            let mut changed = value.clone();
            mutate(at(&mut changed, path), path);
            let mut new: Config = serde_json::from_value(changed).expect(path);
            new.workspace.resolved_workspace_root =
                config.workspace.resolved_workspace_root.clone();
            let plan = classify(&config, &roots, &new, &roots);
            assert_eq!(plan.kind, *expected, "{path}");
        }
        // And the unchanged config is a pure swap.
        assert_eq!(
            classify(&config, &roots, &config, &roots).kind,
            ReloadKind::DisplayOnly
        );
    }
}
