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

/// Keys that are read at query time (or only displayed) and so never need a
/// re-index, as `(section path, key)` pairs of the serialized config.
const QUERY_TIME_KEYS: &[(&[&str], &str)] = &[
    (&["workspace"], "name"),
    (&["workspace"], "version"),
    (&["workspace"], "http_prefix"),
    (&["workspace"], "http_prefixes"),
    (&["engines"], "policy"),
    (&["engines", "docs"], "aliases"),
    (&["engines", "docs"], "stop_words"),
    (&["engines", "docs"], "exact_phrase_boost"),
    (&["engines", "docs"], "fuzzy_fallback"),
];

fn strip(value: &mut Value, section: &[&str], key: &str) {
    let mut cur = value;
    for part in section {
        match cur.get_mut(*part) {
            Some(next) => cur = next,
            None => return,
        }
    }
    if let Some(map) = cur.as_object_mut() {
        map.remove(key);
    }
}

/// The part of `config` that decides what gets indexed: the serialized config
/// without the query-time keys and without custom patterns.
fn index_inputs(config: &Config) -> Option<Value> {
    let mut config = config.clone();
    // An absent `[engines.contracts]` means the section with its defaults, so
    // adding one only to hold patterns is not a change of anything else.
    if config.engines.contracts.is_none() {
        config.engines.contracts = serde_json::from_value(serde_json::json!({})).ok();
    }
    let mut value = serde_json::to_value(&config).ok()?;
    for (section, key) in QUERY_TIME_KEYS {
        strip(&mut value, section, key);
    }
    strip(&mut value, &["engines", "contracts"], "patterns");
    Some(value)
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
}
