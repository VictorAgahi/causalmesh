use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr};
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
        description = "Result granularity: 'symbol' (default) returns one result per declaring symbol; 'package' collapses results to one per distinct (repo, package) pair — use this to see which *services* depend on the target without every individual caller symbol. Any other value is a tool error, not a silent fallback to 'symbol'."
    )]
    pub granularity: Option<CompactStr>,

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
fn validate_granularity(args: &FindDependentsArgs) -> Result<bool, ToolError> {
    match args.granularity.as_deref() {
        None | Some("symbol") => Ok(false),
        Some("package") => Ok(true),
        Some(other) => Err((
            -32602,
            format!(
                "Invalid granularity '{other}' for find_dependents: expected 'symbol' or 'package'."
            ),
        )),
    }
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
    const DESCRIPTION: &'static str = "Resolves the reverse dependency graph across packages, shared modules, gRPC services, and event streams — at symbol granularity by default, or one result per (repo, package) with granularity: 'package'. DO NOT USE to search freeform text or string literals (use smart_search or ripgrep).";
    type Args = FindDependentsArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn truncation_hint(args: &Self::Args, state: &AppState) -> Option<String> {
        // `run` already validated `granularity` (it must have succeeded for this
        // to be called at all) — recomputing here must dedupe the same way, or
        // this hint reports the pre-dedup symbol count for a `granularity:
        // "package"` request whose caller never saw that many results.
        let by_package = validate_granularity(args).ok()?;
        let snapshot = state.snapshot();
        let dependents = snapshot
            .contract_graph
            .find_dependents(args.target.as_str());
        let count = if by_package {
            dedup_by_package(dependents).len()
        } else {
            dependents.len()
        };
        Some(format!(
            "Target '{}' has {} dependent consumer(s). Consider searching for specific caller sub-packages or narrowing your query.",
            args.target,
            count
        ))
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.target.as_str())
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let by_package = validate_granularity(args)?;
        let snapshot = state.snapshot();
        let dependents = snapshot
            .contract_graph
            .find_dependents(args.target.as_str());

        // `granularity: "package"` collapses every dependent down to one
        // result per distinct (repo, package) pair — useful for "which
        // *services* depend on this" without a wall of individual caller
        // symbols, several of which are very often declared in the same
        // package. Default ("symbol") keeps today's one-result-per-symbol
        // behavior unchanged; anything else was already rejected above.
        let dependents = if by_package {
            dedup_by_package(dependents)
        } else {
            dependents
        };

        // Label each result with the root it was crawled from so the
        // formatter can group same-named packages from unrelated services
        // apart instead of flattening them into one undifferentiated list.
        let labeled: Vec<(&_, String)> = dependents
            .into_iter()
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
        let mut text = MarkdownFormatter::format_dependents(args.target.as_str(), &labeled);
        drop(labeled);

        // Plan 4 step 4.1: this tool's scope is every allowed root, so any
        // rejected file that could have declared a dependent (source, proto,
        // YAML — not prose) is named in a note appended last and never cut.
        let note = snapshot.health.scope_note(|p| {
            state.allowed_roots.iter().any(|r| p.starts_with(r)) && may_declare_dependents(p)
        });
        if let Some(note) = note {
            fit_before_note(&mut text, note.len(), dependent_count);
            text.push_str(&note);
        }
        Ok(ToolOutput::text(text))
    }
}

/// Whether a file could hold contract nodes at all: one with a tree-sitter
/// grammar, or a YAML file (OpenAPI/AsyncAPI specs). Keeps an oversized README
/// or lockfile out of every `find_dependents` answer.
fn may_declare_dependents(path: &std::path::Path) -> bool {
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
        assert!(may_declare_dependents(Path::new("/r/svc/api.ts")));
        assert!(may_declare_dependents(Path::new("/r/openapi.YAML")));
        assert!(!may_declare_dependents(Path::new("/r/README.md")));
        assert!(!may_declare_dependents(Path::new("/r/package-lock.json")));
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
