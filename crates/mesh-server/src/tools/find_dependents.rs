use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr, DependentsMatch};
use mesh_parsers::{LanguageKind, MarkdownFormatter, MAX_OUTPUT_BYTES, NON_RESULT_RESERVE_BYTES};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindDependentsArgs {
    #[schemars(
        with = "String",
        description = "Target contract name (ex: 'UserAuthRequest') or package identifier (ex: '@volontariapp/domain-user') to trace reverse dependencies for."
    )]
    pub target: CompactStr,

    #[serde(default)]
    #[schemars(
        with = "String",
        description = "Result granularity: 'symbol' (default) returns one result per dependent declaration (class, interface, function); 'file' one per dependent file; 'package' one per distinct (repo, package) pair — use it to see which *services* depend on the target. Any other value is a tool error, not a silent fallback to 'symbol'."
    )]
    pub granularity: Option<CompactStr>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Include dependents in test files and test-only directories (*.spec.ts, *_test.go, __tests__/, test-utils/, …). Default false: they are left out and counted."
    )]
    pub include_tests: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Maximum number of dependents returned in this page (1-200, default 50). DO NOT raise it to see everything; page with `offset` instead."
    )]
    pub limit: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Number of dependents to skip (default 0). Use the `offset` given in a previous page's footer."
    )]
    pub offset: Option<u32>,

    #[serde(default)]
    // Accepted for W3C trace propagation, hidden from `tools/list`: the model
    // cannot use it, and it cost every session ~200 schema tokens.
    #[schemars(skip)]
    pub _meta: Option<RequestMeta>,
}

/// Rejects an unrecognized `granularity` outright instead of silently falling
/// back to `"symbol"` — a typo'd or invented value (`"Package"`, `"packages"`)
/// would otherwise look like a successful narrower query while quietly
/// returning the full, undeduplicated result set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Granularity {
    Symbol,
    File,
    Package,
}

fn validate_granularity(args: &FindDependentsArgs) -> Result<Granularity, ToolError> {
    match args.granularity.as_deref() {
        None | Some("symbol") => Ok(Granularity::Symbol),
        Some("file") => Ok(Granularity::File),
        Some("package") => Ok(Granularity::Package),
        Some(other) => Err((
            -32602,
            format!(
                "Invalid granularity '{other}' for find_dependents: expected 'symbol', 'file' or 'package'."
            ),
        )),
    }
}

/// Dependents per page when `limit` is omitted, and the most a caller may ask for.
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

/// The full, ordered answer before paging: what `run` renders and
/// `truncation_hint` counts, computed one way for both.
struct Resolved<'g> {
    dependents: Vec<&'g mesh_core::ContractNode>,
    how: DependentsMatch,
    hidden_tests: usize,
}

fn resolve<'g>(
    args: &FindDependentsArgs,
    graph: &'g mesh_core::ContractGraph,
    granularity: Granularity,
) -> Resolved<'g> {
    let (dependents, how) = graph.find_dependents_matched(args.target.as_str());
    let (dependents, tests): (Vec<_>, Vec<_>) = if args.include_tests.unwrap_or(false) {
        (dependents, Vec::new())
    } else {
        dependents
            .into_iter()
            .partition(|n| !mesh_core::is_test_path(&n.file_path))
    };
    let dedup = |nodes| match granularity {
        Granularity::Symbol => nodes,
        Granularity::File => dedup_by_file(nodes),
        Granularity::Package => dedup_by_package(nodes),
    };
    // In the unit of the listing: 7.0.6 counted test *declarations* under a
    // per-file listing ("39 left out" for 34 files).
    let hidden_tests = dedup(tests).len();
    let dependents = dedup(dependents);
    Resolved {
        dependents,
        how,
        hidden_tests,
    }
}

/// One result per file, first declaration seen per file wins.
fn dedup_by_file(dependents: Vec<&mesh_core::ContractNode>) -> Vec<&mesh_core::ContractNode> {
    let mut seen: HashSet<&std::path::Path> = HashSet::new();
    dependents
        .into_iter()
        .filter(|node| seen.insert(&node.file_path))
        .collect()
}

/// One result per distinct `(repo, package)` pair, preserving the input's
/// order (first symbol seen per package wins), so `run` and `truncation_hint`
/// dedupe identically instead of drifting apart.
fn dedup_by_package(dependents: Vec<&mesh_core::ContractNode>) -> Vec<&mesh_core::ContractNode> {
    let mut seen: HashSet<(mesh_core::RepoId, &str)> = HashSet::new();
    dependents
        .into_iter()
        .filter(|node| seen.insert((node.repo_id, node.package.as_str())))
        .collect()
}

pub struct FindDependentsTool;

impl McpTool for FindDependentsTool {
    const NAME: &'static str = "find_dependents";
    const DESCRIPTION: &'static str = "Resolves the reverse dependency graph across packages, shared modules, gRPC services, and event streams: who imports a package (or any subpath of it), a declared symbol's package, or calls an RPC. One result per dependent declaration by default, per file with granularity: 'file', per (repo, package) with granularity: 'package'. Test files are left out unless include_tests: true. Paged with limit/offset. DO NOT USE to search freeform text or string literals (use smart_search or ripgrep).";
    type Args = FindDependentsArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn truncation_hint(args: &Self::Args, state: &AppState) -> Option<String> {
        // `run` already validated `granularity` (it must have succeeded for this
        // to be called at all) — recomputing here must dedupe the same way, or
        // this hint reports the pre-dedup symbol count for a `granularity:
        // "package"` request whose caller never saw that many results.
        let granularity = validate_granularity(args).ok()?;
        let snapshot = state.snapshot();
        let count = resolve(args, &snapshot.contract_graph, granularity)
            .dependents
            .len();
        Some(format!(
            "Target '{}' has {} dependent(s). Page with a smaller `limit`, or use granularity: \"file\" / \"package\".",
            args.target,
            count
        ))
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.target.as_str())
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let granularity = validate_granularity(args)?;
        let limit = args
            .limit
            .map(|l| l.clamp(1, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT) as usize;
        let offset = args.offset.unwrap_or(0) as usize;
        let snapshot = state.snapshot();
        let resolved = resolve(args, &snapshot.contract_graph, granularity);
        let total = resolved.dependents.len();
        let files: HashSet<&std::path::Path> =
            resolved.dependents.iter().map(|n| &*n.file_path).collect();
        let file_count = files.len();
        drop(files);

        // Label each result with the root it was crawled from so the
        // formatter can group same-named packages from unrelated services
        // apart instead of flattening them into one undifferentiated list.
        let labeled: Vec<(&_, String)> = resolved
            .dependents
            .iter()
            .copied()
            .skip(offset)
            .take(limit)
            .map(|node| {
                let label = state
                    .allowed_roots
                    .get(node.repo_id as usize)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unknown root)".to_string());
                (node, label)
            })
            .collect();
        let dependent_count = labeled.len();
        let mut text = String::new();
        if resolved.how == DependentsMatch::Substring {
            text.push_str(&format!(
                "> ⚠ Nothing imports, declares or calls `{}` exactly. The results below are import strings that merely *contain* it (heuristic): check each one.\n\n",
                args.target
            ));
        }
        if labeled.is_empty() && total > 0 {
            // Past the last page: there *are* dependents, just not here
            // (7.0.6 said "No dependents found" at `offset: 180` of 167).
            text.push_str(&format!("## Reverse dependencies of `{}`\n\n", args.target));
        } else {
            text.push_str(&MarkdownFormatter::format_dependents(
                args.target.as_str(),
                &labeled,
            ));
        }
        drop(labeled);
        text.push_str(&page_footer(
            total,
            file_count,
            offset,
            dependent_count,
            resolved.hidden_tests,
            granularity == Granularity::Package,
        ));

        // Plan 4 step 4.1: this tool's scope is every allowed root, so any
        // rejected file that could have declared a dependent (source, proto,
        // YAML — not prose) is named in a note appended last and never cut.
        let note = snapshot.health.scope_note(|p| {
            state.allowed_roots.iter().any(|r| p.starts_with(r)) && is_indexable_source(p)
        });
        if let Some(note) = note {
            fit_before_note(&mut text, note.len(), dependent_count);
            text.push_str(&note);
        }
        Ok(ToolOutput::text(text))
    }
}

/// Totals, paging and what was left out, after the listed results.
fn page_footer(
    total: usize,
    file_count: usize,
    offset: usize,
    shown: usize,
    hidden_tests: usize,
    per_package: bool,
) -> String {
    let mut out = if per_package {
        format!("---\n*{total} dependent package(s)")
    } else {
        format!("---\n*{total} dependent(s) in {file_count} file(s)")
    };
    if total > 0 && offset >= total {
        out.push_str(&format!(
            "; nothing at `offset: {offset}`, request an offset below {total}"
        ));
    } else if total > 0 {
        let first = offset + 1;
        let last = offset + shown;
        out.push_str(&format!("; showing {first}-{last}"));
        if last < total {
            out.push_str(&format!(". More: `offset: {last}`"));
        }
    }
    out.push_str(".*\n");
    if hidden_tests > 0 {
        out.push_str(&format!(
            "*{hidden_tests} more {} in test files left out (`include_tests: true` to list them).*\n",
            if per_package { "package(s)" } else { "dependent(s)" }
        ));
    }
    out
}

/// Whether a file could hold contract nodes at all: one with a tree-sitter
/// grammar, or a YAML file (OpenAPI/AsyncAPI specs). The one filter of the
/// step 4.1 not-indexed note in both `find_dependents` and `smart_search`: it
/// names the source files a search *should* have covered, and keeps an
/// oversized README, a lockfile or an image out of every answer (an image made
/// the note cost 1.8-1.9 KB on every `smart_search` page of the golden
/// corpora). `mesh-mcp doctor` still lists every rejected file.
pub(crate) fn is_indexable_source(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    LanguageKind::from_path(&s).language().is_some()
        || path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
}

/// Cuts `text` at a line boundary so it plus a `note_len`-byte note stays
/// within the payload cap minus [`NON_RESULT_RESERVE_BYTES`], the headroom the
/// registry needs for its own prepended additions (step 4.1 review) — the
/// registry's central truncation, which cuts the *tail*, then never reaches
/// the note. `format_dependents` emits no code fences, so a line cut leaves
/// valid Markdown.
fn fit_before_note(text: &mut String, note_len: usize, total: usize) {
    const CUT_NOTICE_UPPER_BOUND: usize = 256;
    let budget = (MAX_OUTPUT_BYTES - NON_RESULT_RESERVE_BYTES).saturating_sub(note_len);
    if text.len() <= budget {
        return;
    }
    let mut cut = budget.saturating_sub(CUT_NOTICE_UPPER_BOUND);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = text[..cut].rfind('\n').map_or(0, |i| i + 1);
    text.truncate(cut);
    text.push_str(&format!(
        "\n*Output truncated at 48 KB ({total} dependents in total). Narrow with `granularity: \"package\"` or a more specific `target`.*\n"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn only_files_that_can_declare_dependents_are_noted() {
        assert!(is_indexable_source(Path::new("/r/svc/api.ts")));
        assert!(is_indexable_source(Path::new("/r/openapi.YAML")));
        assert!(!is_indexable_source(Path::new("/r/README.md")));
        assert!(!is_indexable_source(Path::new("/r/package-lock.json")));
    }

    #[test]
    fn fit_before_note_leaves_room_for_the_note() {
        let mut text = "[1] `Dep` (Import)\n- **File**: `a.ts:1-2`\n\n".repeat(3_000);
        fit_before_note(&mut text, 2_048, 3_000);
        assert!(
            text.len() + 2_048 <= MAX_OUTPUT_BYTES - 1024,
            "{}",
            text.len()
        );
        assert!(text.ends_with("or a more specific `target`.*\n"), "{text}");
        let mut small = "short\n".to_string();
        fit_before_note(&mut small, 2_048, 1);
        assert_eq!(small, "short\n");
    }
}
