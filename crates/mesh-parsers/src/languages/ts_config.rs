//! Module-specifier aliases of a JavaScript / TypeScript project: `tsconfig.json` /
//! `jsconfig.json` `compilerOptions.paths` (with `baseUrl` and `extends`) and
//! `package.json` `imports` (`#internal/*`).
//!
//! An aliased import (`@/services/User`, `#utils/log`) names no module the graph can
//! find; [`TsAliasIndex::rewrite`] turns it into the relative specifier the file would
//! have written (`../../src/services/User`), which the graph already resolves.
//!
//! Resolution is lazy and per directory: the nearest config above a file is read once
//! per [`TsAliasIndex`] and memoised, so the same code serves a full scan and a
//! one-file incremental reload with no workspace-wide discovery pass, and nothing is
//! cached across `ExtractConfig`s (a rebuilt config re-reads the files). Every file read
//! is confined to the workspace root the importing file belongs to (symlinks and `..`
//! are resolved first), and a file outside every root is never looked up. The cache key
//! of a file's `FileIndex` folds in [`TsAliasIndex::fingerprint_input`], so editing a
//! `tsconfig.json` re-extracts exactly the files that config governs.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// `extends` chains deeper than this are cut (cycle and runaway guard).
const MAX_EXTENDS_DEPTH: usize = 8;
/// A config larger than this is ignored: it is not a hand-written tsconfig.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// One alias: `pattern` (`@/*`, `@app`, `#utils/*`) mapped to `target`, an absolute
/// path that holds a `*` where the pattern does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasRule {
    prefix: String,
    suffix: String,
    wildcard: bool,
    target: String,
}

impl AliasRule {
    /// `None` for a catch-all (`*`, `*.js`): it would claim every bare package
    /// (`react`) and TypeScript itself falls back to `node_modules` after it.
    fn new(pattern: &str, target: String) -> Option<Self> {
        let (prefix, suffix, wildcard) = match pattern.split_once('*') {
            Some((p, s)) => (p, s, true),
            None => (pattern, "", false),
        };
        if prefix.is_empty() {
            return None;
        }
        Some(Self {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
            wildcard,
            target,
        })
    }

    /// The target path `specifier` maps to, if this rule matches it.
    fn apply(&self, specifier: &str) -> Option<String> {
        if !self.wildcard {
            return (specifier == self.prefix).then(|| self.target.clone());
        }
        if specifier.len() < self.prefix.len() + self.suffix.len() {
            return None;
        }
        let rest = specifier.strip_prefix(self.prefix.as_str())?;
        let captured = rest.strip_suffix(self.suffix.as_str())?;
        Some(self.target.replacen('*', captured, 1))
    }
}

/// The rules one config file (with its `extends` chain) defines, longest literal
/// prefix first (TypeScript's choice when several patterns match).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RuleSet {
    rules: Vec<AliasRule>,
    /// Canonical text of `rules`, built once (it keys the cache of every file
    /// the rules govern).
    canonical: String,
}

impl RuleSet {
    fn from_rules(mut rules: Vec<AliasRule>) -> Self {
        rules.sort_by(|a, b| {
            b.prefix
                .len()
                .cmp(&a.prefix.len())
                .then_with(|| a.prefix.cmp(&b.prefix))
                .then_with(|| a.suffix.cmp(&b.suffix))
        });
        rules.dedup();
        let mut canonical = String::new();
        for r in &rules {
            canonical.push_str(&format!(
                "{}|{}|{}|{}\n",
                r.prefix, r.suffix, r.wildcard, r.target
            ));
        }
        Self { rules, canonical }
    }

    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

type Memo = Mutex<HashMap<PathBuf, Option<Arc<RuleSet>>>>;

/// The `tsconfig` and `package.json` rule sets governing one file.
type GoverningRules = (Option<Arc<RuleSet>>, Option<Arc<RuleSet>>);

/// `compilerOptions.paths` entries: pattern -> targets.
type PathEntries = Vec<(String, Vec<String>)>;

struct Inner {
    roots: Vec<PathBuf>,
    /// Nearest `tsconfig.json` / `jsconfig.json` rule set at or above a directory.
    ts: Memo,
    /// Nearest `package.json` `imports` rule set at or above a directory.
    pkg: Memo,
}

/// Alias resolution for a set of workspace roots. `Default` is disabled: it rewrites
/// nothing and contributes nothing to a cache key (unit tests, ad-hoc extraction).
#[derive(Clone, Default)]
pub struct TsAliasIndex {
    inner: Option<Arc<Inner>>,
}

impl std::fmt::Debug for TsAliasIndex {
    /// A constant per state: this value is part of the extraction-config
    /// fingerprint, which must move with neither the memo's contents nor the
    /// set of roots (a root is already part of every file's path, hence its key;
    /// the aliases that govern a file are part of that file's key).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            None => f.write_str("TsAliasIndex(disabled)"),
            Some(_) => f.write_str("TsAliasIndex(enabled)"),
        }
    }
}

impl TsAliasIndex {
    /// Enabled for files under `roots`; a config is never looked for, nor read,
    /// outside the root a file belongs to.
    pub fn new(roots: &[PathBuf]) -> Self {
        // Canonical, so `dir.starts_with(root)` and the jail check agree with the
        // (canonical) paths the crawler yields.
        let mut roots: Vec<PathBuf> = roots
            .iter()
            .map(|r| dunce::canonicalize(r).unwrap_or_else(|_| r.clone()))
            .collect();
        roots.sort();
        roots.dedup();
        Self {
            inner: Some(Arc::new(Inner {
                roots,
                ts: Mutex::new(HashMap::new()),
                pkg: Mutex::new(HashMap::new()),
            })),
        }
    }

    /// The relative specifier `file` would use for the aliased `specifier`, or `None`
    /// when no alias applies (a relative import, a package, no config).
    pub fn rewrite(&self, file: &Path, specifier: &str) -> Option<String> {
        if specifier.starts_with('.') || specifier.starts_with('/') {
            return None;
        }
        let (ts, pkg) = self.rule_sets(file)?;
        let target = [ts, pkg]
            .iter()
            .flatten()
            .flat_map(|set| set.rules.iter())
            .filter_map(|rule| rule.apply(specifier).map(|t| (rule.prefix.len(), t)))
            // Longest literal prefix wins; on a tie the first (tsconfig) rule does.
            .reduce(|best, next| if next.0 > best.0 { next } else { best })?
            .1;
        relative_specifier(file.parent()?, Path::new(&target))
    }

    /// What the aliases governing `file` add to its `FileIndex` cache key; `None`
    /// when none do, so files outside any aliased project keep their key.
    pub fn fingerprint_input(&self, file: &Path) -> Option<String> {
        let (ts, pkg) = self.rule_sets(file)?;
        let mut out = String::new();
        if let Some(ts) = ts {
            out.push_str("ts\n");
            out.push_str(&ts.canonical);
        }
        if let Some(pkg) = pkg {
            out.push_str("pkg\n");
            out.push_str(&pkg.canonical);
        }
        Some(out)
    }

    fn rule_sets(&self, file: &Path) -> Option<GoverningRules> {
        let inner = self.inner.as_ref()?;
        if !is_script_path(file) {
            return None;
        }
        let dir = file.parent()?;
        let ts = inner
            .nearest(&inner.ts, dir, load_ts_rules)
            .filter(|s| !s.is_empty());
        let pkg = inner
            .nearest(&inner.pkg, dir, load_package_imports)
            .filter(|s| !s.is_empty());
        (ts.is_some() || pkg.is_some()).then_some((ts, pkg))
    }
}

fn is_script_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
    )
}

impl Inner {
    /// The rule set of the nearest directory at or above `dir` that holds a config
    /// (`load` returns `Some`, possibly empty: a config with no aliases still
    /// ends the climb, as in TypeScript and Node, so a package never inherits a
    /// parent project's aliases). Never above the file's root, and `None` for a
    /// directory under no root. Every directory climbed is memoised, so a
    /// sibling file costs a lookup.
    fn nearest(
        &self,
        memo: &Memo,
        dir: &Path,
        load: fn(&Path, &Path) -> Option<RuleSet>,
    ) -> Option<Arc<RuleSet>> {
        let jail = self
            .roots
            .iter()
            .filter(|r| dir.starts_with(r))
            .max_by_key(|r| r.components().count())?;
        let mut climbed: Vec<PathBuf> = Vec::new();
        let mut found: Option<Arc<RuleSet>> = None;
        let mut current = Some(dir);
        while let Some(d) = current {
            let cached = memo
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(d)
                .cloned();
            if let Some(hit) = cached {
                found = hit;
                break;
            }
            climbed.push(d.to_path_buf());
            if let Some(set) = load(d, jail) {
                found = Some(Arc::new(set));
                break;
            }
            if d == jail.as_path() {
                break;
            }
            current = d.parent();
        }
        let mut memo = memo.lock().unwrap_or_else(|e| e.into_inner());
        for d in climbed {
            memo.entry(d).or_insert_with(|| found.clone());
        }
        found
    }
}

/// `compilerOptions.paths` of the `tsconfig.json` (else `jsconfig.json`) in `dir`.
/// `Some` as soon as such a config exists, even with no `paths`.
fn load_ts_rules(dir: &Path, jail: &Path) -> Option<RuleSet> {
    for name in ["tsconfig.json", "jsconfig.json"] {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        let layer = load_layer(&path, jail, 0).unwrap_or_default();
        let rules = match layer.paths {
            Some((origin, paths)) => {
                let base = layer.base_url.unwrap_or(origin);
                paths
                    .iter()
                    .filter_map(|(pattern, targets)| {
                        let target = targets.first()?;
                        AliasRule::new(
                            pattern,
                            clean(&base.join(target)).to_string_lossy().into_owned(),
                        )
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        return Some(RuleSet::from_rules(rules));
    }
    None
}

/// `imports` (`"#internal/*": "./src/internal/*.js"`) of the `package.json` in
/// `dir`. `Some` as soon as a `package.json` exists, even with no `imports`.
fn load_package_imports(dir: &Path, jail: &Path) -> Option<RuleSet> {
    let path = dir.join("package.json");
    if !path.exists() {
        return None;
    }
    let value = read_json(&path, jail).unwrap_or(Value::Null);
    let rules = value
        .get("imports")
        .and_then(Value::as_object)
        .map(|imports| {
            imports
                .iter()
                .filter(|(key, _)| key.starts_with('#'))
                .filter_map(|(key, target)| {
                    let target = condition_target(target, 0)?;
                    // A bare target (`"#ext": "some-package"`) points at a package.
                    if !target.starts_with("./") {
                        return None;
                    }
                    AliasRule::new(key, clean(&dir.join(target)).to_string_lossy().into_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    Some(RuleSet::from_rules(rules))
}

/// A `package.json` conditional target: a string, an ordered list of fallbacks, or
/// an object of conditions (`import` / `default` / `require` / `node` preferred).
fn condition_target(value: &Value, depth: usize) -> Option<String> {
    if depth > MAX_EXTENDS_DEPTH {
        return None;
    }
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => items.iter().find_map(|v| condition_target(v, depth + 1)),
        Value::Object(map) => ["import", "default", "require", "node"]
            .iter()
            .find_map(|k| map.get(*k).and_then(|v| condition_target(v, depth + 1)))
            .or_else(|| {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                keys.into_iter()
                    .find_map(|k| condition_target(&map[k], depth + 1))
            }),
        _ => None,
    }
}

/// What a config file (merged with its `extends` chain) contributes.
#[derive(Default)]
struct ConfigLayer {
    /// Absolute `baseUrl`.
    base_url: Option<PathBuf>,
    /// `paths` and the directory of the config that declared them (their base when
    /// there is no `baseUrl`).
    paths: Option<(PathBuf, PathEntries)>,
}

fn load_layer(path: &Path, jail: &Path, depth: usize) -> Option<ConfigLayer> {
    if depth > MAX_EXTENDS_DEPTH {
        return None;
    }
    let value = read_json(path, jail)?;
    let dir = path.parent()?;
    let mut layer = ConfigLayer::default();

    let extends: Vec<&str> = match value.get("extends") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for spec in extends {
        // A package (`@tsconfig/node18`) is not resolved: only relative chains.
        if !(spec.starts_with('.') || spec.starts_with('/')) {
            continue;
        }
        // `./base`, `./base.json` or a directory holding a `tsconfig.json`.
        let named = clean(&dir.join(spec));
        let mut with_json = named.clone().into_os_string();
        with_json.push(".json");
        let parent = [
            named.clone(),
            PathBuf::from(with_json),
            named.join("tsconfig.json"),
        ]
        .into_iter()
        .find(|candidate| candidate.is_file())
        .unwrap_or(named);
        if let Some(inherited) = load_layer(&parent, jail, depth + 1) {
            if inherited.base_url.is_some() {
                layer.base_url = inherited.base_url;
            }
            if inherited.paths.is_some() {
                layer.paths = inherited.paths;
            }
        }
    }

    if let Some(options) = value.get("compilerOptions") {
        if let Some(base) = options.get("baseUrl").and_then(Value::as_str) {
            layer.base_url = Some(clean(&dir.join(base)));
        }
        if let Some(paths) = options.get("paths").and_then(Value::as_object) {
            let mut entries: PathEntries = paths
                .iter()
                .map(|(pattern, targets)| {
                    let targets = targets
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    (pattern.clone(), targets)
                })
                .collect();
            entries.sort();
            layer.paths = Some((dir.to_path_buf(), entries));
        }
    }
    Some(layer)
}

/// Reads a config that lies inside `jail` (a canonical workspace root): the path
/// is resolved first (symlinks, `..`), so a symlinked or `extends`-ed file outside
/// the root is refused. The size cap is enforced on the bytes actually read.
fn read_json(path: &Path, jail: &Path) -> Option<Value> {
    use std::io::Read;
    let canonical = dunce::canonicalize(path).ok()?;
    if !canonical.starts_with(jail) {
        return None;
    }
    // Checked before opening: opening a FIFO for reading blocks until a writer appears.
    if !std::fs::metadata(&canonical).ok()?.is_file() {
        return None;
    }
    let file = std::fs::File::open(&canonical).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return None;
    }
    parse_jsonc(&text)
}

/// Parses JSON with comments and trailing commas (what `tsc` accepts in a tsconfig).
pub(crate) fn parse_jsonc(input: &str) -> Option<Value> {
    serde_json::from_str(&strip_jsonc(input)).ok()
}

/// Removes `//` and `/* */` comments and trailing commas outside string literals,
/// byte-wise so multi-byte UTF-8 passes through untouched.
fn strip_jsonc(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            out.push(b);
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                out.push(c);
                i += 1;
                if c == b'\\' && i < bytes.len() {
                    out.push(bytes[i]);
                    i += 1;
                } else if c == b'"' {
                    break;
                }
            }
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if b == b',' {
            // A trailing comma: the next significant byte closes the container.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if matches!(bytes.get(j), Some(b'}' | b']')) {
                i += 1;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Lexically normalises `path` (`.` and `..` resolved, no filesystem access).
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `target` as an import specifier from a file in `from_dir`: `./x` or `../../x`.
fn relative_specifier(from_dir: &Path, target: &Path) -> Option<String> {
    let from = clean(from_dir);
    let target = clean(target);
    if from.is_absolute() != target.is_absolute() {
        return None;
    }
    let from: Vec<Component> = from.components().collect();
    let target: Vec<Component> = target.components().collect();
    let common = from.iter().zip(&target).take_while(|(a, b)| a == b).count();
    if common == 0
        && from
            .first()
            .is_some_and(|c| matches!(c, Component::Prefix(_)))
    {
        return None;
    }
    let ups = from.len() - common;
    let mut parts: Vec<String> = Vec::new();
    for _ in 0..ups {
        parts.push("..".to_string());
    }
    for c in &target[common..] {
        parts.push(c.as_os_str().to_str()?.to_string());
    }
    let joined = parts.join("/");
    Some(if ups == 0 {
        if joined.is_empty() {
            ".".to_string()
        } else {
            format!("./{joined}")
        }
    } else {
        joined
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce_canonical(tmp.path());
        (tmp, root)
    }

    fn dunce_canonical(p: &Path) -> PathBuf {
        dunce::canonicalize(p).expect("canonicalize")
    }

    #[test]
    fn jsonc_accepts_comments_trailing_commas_and_unicode() {
        let v = parse_jsonc(
            "{\n // c\n \"a\": [1, 2,], /* b */ \"é\": \"x // not a comment\", \"p\": \"q\\\\\",\n}",
        )
        .expect("parse");
        assert_eq!(v["é"], "x // not a comment");
        assert_eq!(v["p"], "q\\");
        assert_eq!(v["a"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn paths_resolve_relative_to_the_importing_file() {
        let (_t, root) = setup();
        write(
            &root,
            "tsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": {
                "@/*": ["src/*"], "@components/*": ["src/ui/components/*"], "@app": ["src/app/index.ts"] } } }"#,
        );
        let idx = TsAliasIndex::new(std::slice::from_ref(&root));
        let file = root.join("src/features/a/page.ts");
        assert_eq!(
            idx.rewrite(&file, "@/services/User").as_deref(),
            Some("../../services/User")
        );
        // Longest literal prefix wins.
        assert_eq!(
            idx.rewrite(&file, "@components/Button").as_deref(),
            Some("../../ui/components/Button")
        );
        // An exact alias matches only itself.
        assert_eq!(
            idx.rewrite(&file, "@app").as_deref(),
            Some("../../app/index.ts")
        );
        assert_eq!(idx.rewrite(&file, "@application"), None);
        assert_eq!(idx.rewrite(&file, "react"), None);
        assert_eq!(idx.rewrite(&file, "./local"), None);
    }

    #[test]
    fn extends_inherits_paths_and_base_url_and_the_child_overrides() {
        let (_t, root) = setup();
        write(
            &root,
            "tsconfig.base.json",
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@lib/*": ["libs/*"] } } }"#,
        );
        write(
            &root,
            "apps/web/tsconfig.json",
            r#"{ "extends": "../../tsconfig.base" }"#,
        );
        write(
            &root,
            "apps/api/tsconfig.json",
            r#"{ "extends": "../../tsconfig.base.json",
                 "compilerOptions": { "baseUrl": "src", "paths": { "~/*": ["*"] } } }"#,
        );
        let idx = TsAliasIndex::new(std::slice::from_ref(&root));
        let web = root.join("apps/web/src/main.ts");
        assert_eq!(
            idx.rewrite(&web, "@lib/log").as_deref(),
            Some("../../../libs/log")
        );
        let api = root.join("apps/api/src/main.ts");
        // `paths` replaces the inherited ones (TypeScript semantics).
        assert_eq!(idx.rewrite(&api, "@lib/log"), None);
        assert_eq!(idx.rewrite(&api, "~/db/pool").as_deref(), Some("./db/pool"));
    }

    #[test]
    fn extends_cycles_and_missing_parents_do_not_hang_or_panic() {
        let (_t, root) = setup();
        write(&root, "a/tsconfig.json", r#"{ "extends": "./b.json" }"#);
        write(&root, "a/b.json", r#"{ "extends": "./tsconfig.json" }"#);
        write(&root, "c/tsconfig.json", r#"{ "extends": "./nope.json" }"#);
        let idx = TsAliasIndex::new(std::slice::from_ref(&root));
        assert_eq!(idx.rewrite(&root.join("a/x.ts"), "@/y"), None);
        assert_eq!(idx.rewrite(&root.join("c/x.ts"), "@/y"), None);
    }

    #[test]
    fn package_json_imports_are_aliases() {
        let (_t, root) = setup();
        write(
            &root,
            "package.json",
            r##"{ "imports": { "#utils/*": "./src/utils/*.js",
                  "#db": { "import": "./src/db/index.mjs", "default": "./src/db/index.cjs" },
                  "#ext": "some-package" } }"##,
        );
        let idx = TsAliasIndex::new(std::slice::from_ref(&root));
        let file = root.join("src/app/main.js");
        assert_eq!(
            idx.rewrite(&file, "#utils/log").as_deref(),
            Some("../utils/log.js")
        );
        assert_eq!(
            idx.rewrite(&file, "#db").as_deref(),
            Some("../db/index.mjs")
        );
        assert_eq!(idx.rewrite(&file, "#ext"), None);
    }

    #[test]
    fn a_catch_all_pattern_never_claims_packages() {
        let (_t, root) = setup();
        write(
            &root,
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "*": ["types/*"], "@x/*": ["x/*"] } } }"#,
        );
        let idx = TsAliasIndex::new(std::slice::from_ref(&root));
        let file = root.join("src/a.ts");
        assert_eq!(idx.rewrite(&file, "react"), None);
        assert_eq!(idx.rewrite(&file, "@x/y").as_deref(), Some("../x/y"));
    }

    #[test]
    fn config_is_never_taken_from_above_the_root() {
        let (_t, root) = setup();
        write(
            &root,
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
        );
        let inner_root = root.join("services/billing");
        std::fs::create_dir_all(&inner_root).expect("mkdir");
        let idx = TsAliasIndex::new(std::slice::from_ref(&inner_root));
        assert_eq!(idx.rewrite(&inner_root.join("a.ts"), "@/y"), None);
    }

    #[test]
    fn files_are_fingerprinted_by_the_aliases_governing_them() {
        let (_t, root) = setup();
        write(
            &root,
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
        );
        write(&root, "other/readme.md", "x");
        let idx = TsAliasIndex::new(std::slice::from_ref(&root));
        let ts = idx
            .fingerprint_input(&root.join("src/a.ts"))
            .expect("aliased");
        assert_eq!(
            Some(ts.clone()),
            idx.fingerprint_input(&root.join("src/deep/b.tsx"))
        );
        assert_eq!(idx.fingerprint_input(&root.join("other/readme.md")), None);
        assert_eq!(
            TsAliasIndex::default().fingerprint_input(&root.join("src/a.ts")),
            None
        );

        write(
            &root,
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["lib/*"] } } }"#,
        );
        let changed = TsAliasIndex::new(std::slice::from_ref(&root));
        assert_ne!(changed.fingerprint_input(&root.join("src/a.ts")), Some(ts));
    }

    #[test]
    fn debug_output_is_stable_whatever_the_memo_holds() {
        let (_t, root) = setup();
        write(
            &root,
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
        );
        let idx = TsAliasIndex::new(std::slice::from_ref(&root));
        let before = format!("{idx:?}");
        let _ = idx.rewrite(&root.join("src/a.ts"), "@/b");
        assert_eq!(before, format!("{idx:?}"));
    }

    #[test]
    fn relative_specifier_shapes() {
        let r = |from: &str, to: &str| relative_specifier(Path::new(from), Path::new(to));
        assert_eq!(r("/a/b", "/a/b/c/d").as_deref(), Some("./c/d"));
        assert_eq!(r("/a/b/c", "/a/x").as_deref(), Some("../../x"));
        assert_eq!(r("/a", "/a").as_deref(), Some("."));
        assert_eq!(r("/a", "b"), None);
    }

    #[test]
    fn extends_outside_the_root_is_refused() {
        let (_t, root) = setup();
        let outside = tempfile::tempdir().expect("tempdir");
        let outside_dir = dunce::canonicalize(outside.path()).expect("canon");
        write(
            &outside_dir,
            "evil.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["secret/*"] } } }"#,
        );
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).expect("mkdir");
        // Absolute and relative `extends` pointing outside the workspace root.
        let config = |extends: &Path| {
            serde_json::json!({ "extends": extends.to_string_lossy() }).to_string()
        };
        write(
            &ws,
            "abs/tsconfig.json",
            &config(&outside_dir.join("evil.json")),
        );
        let mut up = PathBuf::new();
        for _ in 0..ws.join("rel").components().count() {
            up.push("..");
        }
        let relative = up.join(
            outside_dir
                .components()
                .filter(|c| matches!(c, Component::Normal(_)))
                .collect::<PathBuf>(),
        );
        write(
            &ws,
            "rel/tsconfig.json",
            &config(&relative.join("evil.json")),
        );
        let idx = TsAliasIndex::new(std::slice::from_ref(&ws));
        assert_eq!(idx.rewrite(&ws.join("abs/a.ts"), "@/x"), None);
        assert_eq!(idx.rewrite(&ws.join("rel/a.ts"), "@/x"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_leaving_the_root_is_refused() {
        let (_t, root) = setup();
        let outside = tempfile::tempdir().expect("tempdir");
        let outside_dir = dunce::canonicalize(outside.path()).expect("canon");
        write(
            &outside_dir,
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["secret/*"] } } }"#,
        );
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).expect("mkdir");
        std::os::unix::fs::symlink(outside_dir.join("tsconfig.json"), ws.join("tsconfig.json"))
            .expect("symlink");
        write(
            &ws,
            "inside.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
        );
        std::fs::create_dir_all(ws.join("pkg")).expect("mkdir");
        std::os::unix::fs::symlink(ws.join("inside.json"), ws.join("pkg/tsconfig.json"))
            .expect("symlink");
        let idx = TsAliasIndex::new(std::slice::from_ref(&ws));
        assert_eq!(idx.rewrite(&ws.join("a.ts"), "@/x"), None);
        // A symlink that stays inside the root is fine.
        assert_eq!(
            idx.rewrite(&ws.join("pkg/a.ts"), "@/x").as_deref(),
            Some("./src/x")
        );
    }

    #[test]
    fn a_non_canonical_root_is_canonicalised_and_a_file_outside_every_root_is_ignored() {
        let (_t, root) = setup();
        write(
            &root,
            "ws/tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
        );
        write(
            &root,
            "other/tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
        );
        write(&root, "ws/src/a.ts", "");
        let dotted = root.join("ws/src/..");
        let idx = TsAliasIndex::new(std::slice::from_ref(&dotted));
        assert!(idx.rewrite(&root.join("ws/src/a.ts"), "@/b").is_some());
        assert_eq!(idx.rewrite(&root.join("other/a.ts"), "@/b"), None);
    }

    #[test]
    fn a_nested_config_without_aliases_ends_the_climb() {
        let (_t, root) = setup();
        write(
            &root,
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
        );
        write(
            &root,
            "package.json",
            r##"{ "imports": { "#u/*": "./u/*.js" } }"##,
        );
        write(
            &root,
            "packages/b/tsconfig.json",
            r#"{ "compilerOptions": { "strict": true } }"#,
        );
        write(&root, "packages/b/package.json", r#"{ "name": "b" }"#);
        write(&root, "packages/c/src/a.ts", "");
        let idx = TsAliasIndex::new(std::slice::from_ref(&root));
        let b = root.join("packages/b/src/a.ts");
        assert_eq!(idx.rewrite(&b, "@/y"), None);
        assert_eq!(idx.rewrite(&b, "#u/y"), None);
        assert_eq!(idx.fingerprint_input(&b), None);
        // A package with no config of its own is still governed by the root's.
        assert!(idx
            .rewrite(&root.join("packages/c/src/a.ts"), "@/y")
            .is_some());
    }

    #[test]
    fn extends_may_name_a_directory_and_the_debug_form_ignores_the_roots() {
        let (_t, root) = setup();
        write(
            &root,
            "base/tsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@b/*": ["lib/*"] } } }"#,
        );
        write(&root, "app/tsconfig.json", r#"{ "extends": "../base" }"#);
        let idx = TsAliasIndex::new(std::slice::from_ref(&root));
        assert_eq!(
            idx.rewrite(&root.join("app/src/a.ts"), "@b/x").as_deref(),
            Some("../../base/lib/x")
        );
        let other = TsAliasIndex::new(&[root.join("app"), root.join("base")]);
        assert_eq!(format!("{idx:?}"), format!("{other:?}"));
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_named_like_a_config_does_not_hang_the_indexer() {
        let (_t, root) = setup();
        for name in ["tsconfig.json", "package.json"] {
            let dir = root.join(name.trim_end_matches(".json"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let status = std::process::Command::new("mkfifo")
                .arg(dir.join(name))
                .status()
                .expect("mkfifo");
            assert!(status.success());
            let idx = TsAliasIndex::new(std::slice::from_ref(&root));
            let file = dir.join("a.ts");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(idx.rewrite(&file, "@/x"));
            });
            let result = rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("a FIFO config must be skipped, not opened");
            assert_eq!(result, None);
        }
    }
}
