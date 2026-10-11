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
//! cached across `ExtractConfig`s (a rebuilt config re-reads the files). The cache key
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
/// Directories climbed from a file that sits under no known root.
const MAX_CLIMB: usize = 16;

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
        Self { rules }
    }

    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// A canonical text of the rules, for cache keys.
    fn canonical(&self) -> String {
        let mut out = String::new();
        for r in &self.rules {
            out.push_str(&format!(
                "{}|{}|{}|{}\n",
                r.prefix, r.suffix, r.wildcard, r.target
            ));
        }
        out
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
    /// Roots only: this value is part of the extraction-config fingerprint, which
    /// must not move with the memo's contents.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            None => f.write_str("TsAliasIndex(disabled)"),
            Some(inner) => write!(f, "TsAliasIndex(roots={:?})", inner.roots),
        }
    }
}

impl TsAliasIndex {
    /// Enabled for files under `roots`; a config is never looked for above the
    /// root a file belongs to.
    pub fn new(roots: &[PathBuf]) -> Self {
        let mut roots = roots.to_vec();
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
            out.push_str(&ts.canonical());
        }
        if let Some(pkg) = pkg {
            out.push_str("pkg\n");
            out.push_str(&pkg.canonical());
        }
        (!out.is_empty()).then_some(out)
    }

    fn rule_sets(&self, file: &Path) -> Option<GoverningRules> {
        let inner = self.inner.as_ref()?;
        if !is_script_path(file) {
            return None;
        }
        let dir = file.parent()?;
        let ts = inner.nearest(&inner.ts, dir, load_ts_rules);
        let pkg = inner.nearest(&inner.pkg, dir, load_package_imports);
        (ts.is_some() || pkg.is_some()).then_some((ts, pkg))
    }
}

fn is_script_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(
            "ts" | "tsx"
                | "mts"
                | "cts"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "vue"
                | "svelte"
                | "astro"
        )
    )
}

impl Inner {
    /// The rule set of the nearest directory at or above `dir` (never above the
    /// file's root) for which `load` finds one, memoised for every directory
    /// climbed so a sibling file costs a lookup.
    fn nearest(
        &self,
        memo: &Memo,
        dir: &Path,
        load: fn(&Path) -> Option<RuleSet>,
    ) -> Option<Arc<RuleSet>> {
        let boundary = self
            .roots
            .iter()
            .filter(|r| dir.starts_with(r))
            .max_by_key(|r| r.components().count());
        let mut climbed: Vec<PathBuf> = Vec::new();
        let mut found: Option<Arc<RuleSet>> = None;
        let mut current = Some(dir);
        while let Some(d) = current {
            if climbed.len() >= MAX_CLIMB && boundary.is_none() {
                break;
            }
            let cached = memo
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(d)
                .cloned();
            match cached {
                Some(hit) => {
                    found = hit;
                    break;
                }
                None => {
                    climbed.push(d.to_path_buf());
                    if let Some(set) = load(d).filter(|s| !s.is_empty()) {
                        found = Some(Arc::new(set));
                        break;
                    }
                }
            }
            if boundary.is_some_and(|b| b.as_path() == d) {
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

/// `compilerOptions.paths` of the `tsconfig.json` / `jsconfig.json` in `dir`.
fn load_ts_rules(dir: &Path) -> Option<RuleSet> {
    for name in ["tsconfig.json", "jsconfig.json"] {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        let Some(layer) = load_layer(&path, 0) else {
            continue;
        };
        let Some((origin, paths)) = layer.paths else {
            continue;
        };
        let base = layer.base_url.unwrap_or(origin);
        let rules = paths
            .iter()
            .filter_map(|(pattern, targets)| {
                let target = targets.first()?;
                AliasRule::new(
                    pattern,
                    clean(&base.join(target)).to_string_lossy().into_owned(),
                )
            })
            .collect();
        let set = RuleSet::from_rules(rules);
        if !set.is_empty() {
            return Some(set);
        }
    }
    None
}

/// `imports` (`"#internal/*": "./src/internal/*.js"`) of the `package.json` in `dir`.
fn load_package_imports(dir: &Path) -> Option<RuleSet> {
    let path = dir.join("package.json");
    let value = read_json(&path)?;
    let imports = value.get("imports")?.as_object()?;
    let rules = imports
        .iter()
        .filter(|(key, _)| key.starts_with('#'))
        .filter_map(|(key, target)| {
            let target = condition_target(target, 0)?;
            // A bare target (`"#ext": "some-package"`) points at a package, not a file.
            if !target.starts_with("./") {
                return None;
            }
            AliasRule::new(key, clean(&dir.join(target)).to_string_lossy().into_owned())
        })
        .collect();
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

fn load_layer(path: &Path, depth: usize) -> Option<ConfigLayer> {
    if depth > MAX_EXTENDS_DEPTH {
        return None;
    }
    let value = read_json(path)?;
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
        let mut parent = clean(&dir.join(spec));
        if !parent.is_file() {
            parent.as_mut_os_string().push(".json");
        }
        if let Some(inherited) = load_layer(&parent, depth + 1) {
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

fn read_json(path: &Path) -> Option<Value> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    parse_jsonc(&text)
}

/// Parses JSON with comments and trailing commas (what `tsc` accepts in a tsconfig).
pub fn parse_jsonc(input: &str) -> Option<Value> {
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
        std::fs::canonicalize(p).expect("canonicalize")
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
}
