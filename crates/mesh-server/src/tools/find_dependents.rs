use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr, DependentsMatch};
use mesh_parsers::{LanguageKind, MarkdownFormatter, MAX_OUTPUT_BYTES, NON_RESULT_RESERVE_BYTES};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

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
        description = "Result granularity: 'symbol' (default) returns one result per dependent declaration (class, interface, function); 'file' one per dependent file; 'package' one per distinct (repo, package) pair, with file and symbol counts and up to 3 representative locations — use it to see which *services* depend on the target. Any other value is a tool error, not a silent fallback to 'symbol'."
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
    /// Symbol and file granularity: the listed nodes. Empty in package mode.
    dependents: Vec<&'g mesh_core::ContractNode>,
    /// Package granularity: the listed packages. Empty otherwise.
    packages: Vec<PackageGroup<'g>>,
    per_package: bool,
    how: DependentsMatch,
    hidden_tests: usize,
}

impl Resolved<'_> {
    /// Number of listed units, in the unit of the requested granularity.
    fn total(&self) -> usize {
        if self.per_package {
            self.packages.len()
        } else {
            self.dependents.len()
        }
    }
}

/// One dependent package: the unit of a `granularity: "package"` answer.
struct PackageGroup<'g> {
    repo_id: mesh_core::RepoId,
    package: &'g str,
    /// Distinct files of the package that use the target.
    files: usize,
    /// Declarations of the package that use the target (a bare file with
    /// imports and no declaration of its own counts as a file only).
    symbols: usize,
    /// Every dependent declaration of the package, unordered.
    nodes: Vec<&'g mesh_core::ContractNode>,
    /// Up to [`MAX_REPRESENTATIVES`] locations, each in a different file. Empty
    /// until [`pick_representatives`] ran: only the packages on the requested
    /// page pay for it.
    reps: Vec<&'g mesh_core::ContractNode>,
}

/// Representative locations shown per dependent package.
const MAX_REPRESENTATIVES: usize = 3;

/// Resolving is the same cheap computation `truncation_hint` repeats for its
/// count (the tool trait gives it no access to `run`'s result): no graph-wide
/// scan happens here, ranking waits for the page ([`pick_representatives`]).
fn resolve<'g>(
    args: &FindDependentsArgs,
    graph: &'g mesh_core::ContractGraph,
    granularity: Granularity,
    roots: &[std::path::PathBuf],
) -> Resolved<'g> {
    let (dependents, how) = graph.find_dependents_matched(args.target.as_str());
    let (dependents, tests): (Vec<_>, Vec<_>) = if args.include_tests.unwrap_or(false) {
        (dependents, Vec::new())
    } else {
        dependents
            .into_iter()
            .partition(|n| !mesh_core::is_test_node(n, roots))
    };
    if granularity == Granularity::Package {
        let packages = group_by_package(dependents);
        // Packages whose every dependent is test code: a package with a test
        // and a production dependent is listed, not "left out".
        let listed: HashSet<(mesh_core::RepoId, &str)> =
            packages.iter().map(|g| (g.repo_id, g.package)).collect();
        let hidden_tests = tests
            .iter()
            .map(|n| (n.repo_id, n.package.as_str()))
            .collect::<HashSet<_>>()
            .difference(&listed)
            .count();
        return Resolved {
            dependents: Vec::new(),
            packages,
            per_package: true,
            how,
            hidden_tests,
        };
    }
    let dedup = |nodes| match granularity {
        Granularity::File => dedup_by_file(nodes),
        _ => nodes,
    };
    // In the unit of the listing: 7.0.6 counted test *declarations* under a
    // per-file listing ("39 left out" for 34 files).
    let hidden_tests = dedup(tests).len();
    let dependents = dedup(dependents);
    Resolved {
        dependents,
        packages: Vec::new(),
        per_package: false,
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

/// `node`'s file path relative to its workspace root, like `is_test_node`
/// judges it: a checkout living under a directory named `generated` or `gen`
/// must not rank every file as generated.
fn root_relative<'n>(
    node: &'n mesh_core::ContractNode,
    roots: &[std::path::PathBuf],
) -> &'n std::path::Path {
    roots
        .get(node.repo_id as usize)
        .and_then(|r| node.file_path.strip_prefix(r).ok())
        .unwrap_or(&node.file_path)
}

/// Groups dependents by `(repo, package)`: the package is the unit, with the
/// number of files and declarations that use the target. Cheap (one pass over
/// the dependents, no graph scan), so the count of packages is too. Everything
/// here is a total order, so the answer does not depend on hash or graph
/// iteration order: packages by files desc, declarations desc, then
/// `(repo, package)`. Their locations come later, for the listed page only
/// ([`pick_representatives`]).
fn group_by_package<'g>(dependents: Vec<&'g mesh_core::ContractNode>) -> Vec<PackageGroup<'g>> {
    let mut by_package: BTreeMap<(mesh_core::RepoId, &str), Vec<&mesh_core::ContractNode>> =
        BTreeMap::new();
    for node in dependents {
        by_package
            .entry((node.repo_id, node.package.as_str()))
            .or_default()
            .push(node);
    }
    let mut groups: Vec<PackageGroup<'g>> = by_package
        .into_iter()
        .map(|((repo_id, package), nodes)| {
            let files = nodes
                .iter()
                .map(|n| &*n.file_path)
                .collect::<HashSet<_>>()
                .len();
            let symbols = nodes
                .iter()
                .filter(|n| n.kind != mesh_core::NodeKind::Module)
                .count();
            PackageGroup {
                repo_id,
                package,
                files,
                symbols,
                nodes,
                reps: Vec::new(),
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.files
            .cmp(&a.files)
            .then_with(|| b.symbols.cmp(&a.symbols))
            .then_with(|| a.repo_id.cmp(&b.repo_id))
            .then_with(|| a.package.cmp(b.package))
    });
    groups
}

/// Fills `reps` of `page`: up to [`MAX_REPRESENTATIVES`] locations per package,
/// each in a different file, preferring production over test code,
/// hand-written over generated, a declaration over a bare import-only file,
/// then the most referenced declaration (incoming graph edges, counted in one
/// pass over the edges and only for the declarations of these packages), then
/// path and line.
fn pick_representatives<'g>(
    page: &mut [PackageGroup<'g>],
    graph: &'g mesh_core::ContractGraph,
    roots: &[std::path::PathBuf],
) {
    if page.is_empty() {
        return;
    }
    let candidates: HashSet<mesh_core::NodeId> = page
        .iter()
        .flat_map(|g| g.nodes.iter().map(|n| n.id))
        .collect();
    let mut referenced: HashMap<mesh_core::NodeId, usize> = HashMap::new();
    for edge in graph.all_edges() {
        if candidates.contains(&edge.to) {
            *referenced.entry(edge.to).or_default() += 1;
        }
    }
    for group in page {
        let mut nodes = std::mem::take(&mut group.nodes);
        let rank = |n: &&mesh_core::ContractNode| {
            let rel = root_relative(n, roots);
            (
                mesh_core::is_test_path(rel),
                mesh_core::is_generated_path(rel),
                n.kind == mesh_core::NodeKind::Module,
                std::cmp::Reverse(referenced.get(&n.id).copied().unwrap_or(0)),
            )
        };
        nodes.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.file_path.cmp(&b.file_path))
                .then_with(|| a.line_start.cmp(&b.line_start))
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut seen_files: HashSet<&std::path::Path> = HashSet::new();
        group.reps = nodes
            .into_iter()
            .filter(|n| seen_files.insert(&n.file_path))
            .take(MAX_REPRESENTATIVES)
            .collect();
    }
}

/// Renders one page of dependent packages: the package (with its root) is the
/// unit, then how many files and declarations use the target, then a few
/// representative `file:line` locations each with the declaration's own kind.
fn format_packages(
    target: &str,
    page: &[PackageGroup<'_>],
    first_index: usize,
    roots: &[std::path::PathBuf],
) -> String {
    let mut out = format!("## Dependent packages of `{target}`\n\n");
    for (i, group) in page.iter().enumerate() {
        let root = roots
            .get(group.repo_id as usize)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unknown root)".to_string());
        out.push_str(&format!(
            "[{}] `{}` (root `{root}`): {} file(s), {} symbol(s) use it\n",
            first_index + i,
            group.package,
            group.files,
            group.symbols
        ));
        for node in &group.reps {
            out.push_str(&format!(
                "- `{}`{} `{}:{}`\n",
                node.name,
                mesh_parsers::declaration_label(node)
                    .map(|l| format!(" ({l})"))
                    .unwrap_or_default(),
                root_relative(node, roots)
                    .display()
                    .to_string()
                    .replace('\\', "/"),
                node.line_start
            ));
        }
        let more = group.files.saturating_sub(group.reps.len());
        if more > 0 {
            out.push_str(&format!("- ... and {more} more file(s)\n"));
        }
        out.push('\n');
    }
    out
}

pub struct FindDependentsTool;

impl McpTool for FindDependentsTool {
    const NAME: &'static str = "find_dependents";
    const DESCRIPTION: &'static str = "Authoritative reverse dependency graph across packages, shared modules, gRPC services, and event streams: answers who imports or uses a package, module or symbol (imports and symbol usage). For 'who calls this gRPC method / RPC client call sites' use analyze_grpc. If 0 dependents are reported, no indexed import or call resolves to it; only dynamic or string-built references can be missing, so a targeted check of those beats a broad grep. One result per dependent declaration by default, per file with granularity: 'file', per (repo, package) with granularity: 'package'. Test files are left out unless include_tests: true. Paged with limit/offset. DO NOT USE to search freeform text or string literals (use smart_search or ripgrep).";
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
        let count = resolve(
            args,
            &snapshot.contract_graph,
            granularity,
            &state.allowed_roots,
        )
        .total();
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
        let mut resolved = resolve(
            args,
            &snapshot.contract_graph,
            granularity,
            &state.allowed_roots,
        );
        let total = resolved.total();
        // Locations only for the packages on this page.
        let page_start = offset.min(resolved.packages.len());
        let page_end = page_start
            .saturating_add(limit)
            .min(resolved.packages.len());
        pick_representatives(
            &mut resolved.packages[page_start..page_end],
            &snapshot.contract_graph,
            &state.allowed_roots,
        );
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
        let package_page: &[PackageGroup<'_>] = &resolved.packages[page_start..page_end];
        let dependent_count = if resolved.per_package {
            package_page.len()
        } else {
            labeled.len()
        };
        let mut text = String::new();
        if resolved.how == DependentsMatch::Substring {
            text.push_str(&format!(
                "> ⚠ Nothing imports, declares or calls `{}` exactly. The results below are import strings that merely *contain* it (heuristic): check each one.\n\n",
                args.target
            ));
        }
        if dependent_count == 0 && total > 0 {
            // Past the last page: there *are* dependents, just not here
            // (7.0.6 said "No dependents found" at `offset: 180` of 167).
            text.push_str(&format!("## Reverse dependencies of `{}`\n\n", args.target));
        } else if resolved.per_package && total > 0 {
            text.push_str(&format_packages(
                args.target.as_str(),
                package_page,
                offset + 1,
                &state.allowed_roots,
            ));
        } else {
            // The files the index parsed, the figure `doctor` reports as
            // indexed. The graph's own `file_count()` only counts files that
            // carry a contract node and read as a contradiction next to it.
            let files_scanned = snapshot.health.files_indexed;
            text.push_str(&MarkdownFormatter::format_dependents(
                args.target.as_str(),
                &labeled,
                files_scanned,
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
