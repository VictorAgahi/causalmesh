use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Most rejected files [`IndexHealth`] remembers by path. Past it the counters
/// keep counting, but only `rejected_overflow` records how many paths were dropped.
pub const MAX_REJECTED_FILES: usize = 10_000;

/// Most rejected files one tool note lists before folding the rest into "and N more".
pub const NOTE_MAX_FILES: usize = 20;

/// Hard byte ceiling of one tool note ([`IndexHealth::scope_note`]).
pub const NOTE_MAX_BYTES: usize = 2 * 1024;

/// Why a file is missing from (or empty in) the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RejectReason {
    /// Over the 384KB/1.5MB size budget.
    Oversized,
    /// Failed a lexical pre-check (binary sniff, long line, deep nesting).
    Guard,
    /// Unreadable or not valid UTF-8.
    ReadError,
    /// Passed every guard, but tree-sitter failed twice: indexed with no facts
    /// (full scan) or with its last known-good facts (incremental reload).
    ParseFailed,
}

impl RejectReason {
    pub const ALL: [RejectReason; 4] = [
        RejectReason::Oversized,
        RejectReason::Guard,
        RejectReason::ReadError,
        RejectReason::ParseFailed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RejectReason::Oversized => "oversized",
            RejectReason::Guard => "binary/long line/deep nesting",
            RejectReason::ReadError => "unreadable",
            RejectReason::ParseFailed => "parse failed",
        }
    }
}

/// One file the indexer did not fold into the graph, as it was on disk when rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedFile {
    pub path: PathBuf,
    pub reason: RejectReason,
    pub size_bytes: u64,
}

/// Counts of what happened to every file the indexer looked at, since the last
/// full rebuild (a fresh boot scan or `mesh-mcp graph` run zeroes it; each
/// incremental reload adds the outcome of the files it reprocessed).
///
/// Exists so a file that didn't make it into the graph is always visible
/// somewhere (`mesh-mcp doctor`, a tool output footer) instead of being
/// indistinguishable from a legitimately empty file — the silent-failure half
/// of idempotence invariant I6. `files_parse_failed` in particular should be
/// at (or very near) zero in practice: `AstGuard::INDEX_PARSE_TIMEOUT_MICROS`
/// gives 2s of headroom, and a file that reaches it survived every pre-parse
/// guard (size, binary sniff, line length, nesting) and still failed a
/// same-thread, uncontended retry.
///
/// Beyond the counters, `rejected` names the files themselves (plan 4 step
/// 4.1), so a tool can say *which* file in its scope it could not search:
/// sorted by path, one entry per path, at most [`MAX_REJECTED_FILES`]. Unlike
/// the counters it is exact after an incremental reload — every path a reload
/// reprocessed or deleted is replaced by that pass's own outcome
/// ([`IndexHealth::replace_rejected`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexHealth {
    /// Every file the crawler handed to `process_file`, whatever happened next.
    pub files_scanned: usize,
    /// Read, guarded, parsed and folded into the graph — including a file whose
    /// tree-sitter parse failed on the first (contended) attempt but succeeded
    /// on the sequential retry, and one that legitimately produced zero facts.
    pub files_indexed: usize,
    /// Rejected before ever reaching a parser: over the 384KB/1.5MB size budget.
    pub files_rejected_oversized: usize,
    /// Rejected by a lexical pre-check: a null byte in the first 4KB, a line over
    /// 1024 bytes, syntactic nesting over depth 64, or (non-tree-sitter files)
    /// the binary sniff alone.
    pub files_rejected_guard: usize,
    /// The file could not be read (permissions, vanished mid-scan, not valid UTF-8).
    pub files_read_error: usize,
    /// Tree-sitter returned no tree even on a same-thread, uncontended retry —
    /// the file is indexed with zero facts, and this is the one count that
    /// should almost always read zero. See the type's own doc for why.
    pub files_parse_failed: usize,
    /// Every rejected file by path (see the type's doc). Sorted by path, deduplicated.
    #[serde(default)]
    pub rejected: Vec<RejectedFile>,
    /// Rejected paths dropped from `rejected` by the [`MAX_REJECTED_FILES`] cap.
    #[serde(default)]
    pub rejected_overflow: usize,
}

impl IndexHealth {
    #[inline]
    pub fn record_scanned(&mut self, n: usize) {
        self.files_scanned += n;
    }

    #[inline]
    pub fn record_indexed(&mut self) {
        self.files_indexed += 1;
    }

    #[inline]
    pub fn record_oversized(&mut self) {
        self.files_rejected_oversized += 1;
    }

    #[inline]
    pub fn record_guard_rejected(&mut self) {
        self.files_rejected_guard += 1;
    }

    #[inline]
    pub fn record_read_error(&mut self) {
        self.files_read_error += 1;
    }

    #[inline]
    pub fn record_parse_failed(&mut self) {
        self.files_parse_failed += 1;
    }

    /// Remembers one rejected file. Call [`Self::normalize_rejected`] once the
    /// pass is done to restore the sorted, deduplicated, bounded invariant.
    pub fn record_rejected_file(&mut self, path: PathBuf, reason: RejectReason, size_bytes: u64) {
        self.rejected.push(RejectedFile {
            path,
            reason,
            size_bytes,
        });
    }

    /// Sorts `rejected` by path, keeps the last entry recorded per path, and
    /// applies the [`MAX_REJECTED_FILES`] cap (lowest paths kept, so the kept
    /// set is deterministic whatever order the pool finished in).
    pub fn normalize_rejected(&mut self) {
        // Stable sort: equal paths keep insertion order, so the latest wins below.
        self.rejected.sort_by(|a, b| a.path.cmp(&b.path));
        let mut out: Vec<RejectedFile> = Vec::with_capacity(self.rejected.len());
        for r in self.rejected.drain(..) {
            match out.last_mut() {
                Some(last) if last.path == r.path => *last = r,
                _ => out.push(r),
            }
        }
        // Never accumulated (`+=` here drifted upward without bound over a long-running
        // daemon's life: called on every incremental reload via `replace_rejected`, step
        // 4.1 review) and never simply reassigned either: once a path is truncated off
        // `self.rejected`, it is gone for good, so a later incremental call only ever
        // sees the (already capped) survivors plus this pass's own new rejections — an
        // outright reassignment would silently drop back toward 0 the very next reload
        // even though the originally dropped paths are still out there, still rejected.
        // `max` keeps the high-water mark until the next full rebuild (a fresh
        // `IndexHealth::default()`, see the type's doc) legitimately recomputes it from
        // an unbounded, not-yet-truncated list.
        self.rejected_overflow = self
            .rejected_overflow
            .max(out.len().saturating_sub(MAX_REJECTED_FILES));
        out.truncate(MAX_REJECTED_FILES);
        self.rejected = out;
    }

    /// Incremental reload: every path in `touched` (reprocessed or deleted by
    /// this pass) loses its old entry, and `fresh` — this pass's own rejections
    /// — is added. A file rejected before and valid now disappears; a new
    /// rejection appears. Returns whether the list changed.
    pub fn replace_rejected(&mut self, touched: &HashSet<&Path>, fresh: &[RejectedFile]) -> bool {
        let before = self.rejected.clone();
        self.rejected
            .retain(|r| !touched.contains(r.path.as_path()));
        self.rejected.extend(fresh.iter().cloned());
        self.normalize_rejected();
        self.rejected != before
    }

    /// Rejected files for which `keep` holds (e.g. inside a query's scope), in path order.
    pub fn rejected_matching<'a>(
        &'a self,
        keep: impl Fn(&Path) -> bool + 'a,
    ) -> impl Iterator<Item = &'a RejectedFile> + 'a {
        self.rejected.iter().filter(move |r| keep(&r.path))
    }

    /// The note a search tool appends when files it could not search sit
    /// inside its scope: `None` when there are none. At most
    /// [`NOTE_MAX_FILES`] files and [`NOTE_MAX_BYTES`] bytes; the rest folds
    /// into "and N more". Callers reserve its length *before* laying out their
    /// results, then append it last, whole.
    pub fn scope_note(&self, keep: impl Fn(&Path) -> bool) -> Option<String> {
        let hits: Vec<&RejectedFile> = self.rejected_matching(keep).collect();
        if hits.is_empty() {
            return None;
        }
        let mut note = format!(
            "\n> [!WARNING]\n> **{} file(s) in this scope are not indexed**, so the results above cannot include them. \
             If what you are looking for may live there, read the file directly instead of searching again.\n",
            hits.len()
        );
        // Room kept for the final "and N more" line (and the cap notice).
        const TAIL_RESERVE: usize = 160;
        let mut shown = 0;
        for r in hits.iter().take(NOTE_MAX_FILES) {
            let line = format!(
                "> - `{}`: {}, {}\n",
                sanitize_path(&r.path),
                r.reason.label(),
                human_size(r.size_bytes)
            );
            if note.len() + line.len() + TAIL_RESERVE > NOTE_MAX_BYTES {
                break;
            }
            note.push_str(&line);
            shown += 1;
        }
        let rest = hits.len() - shown;
        if rest > 0 {
            let _ = writeln!(note, "> - …and {rest} more");
        }
        if self.rejected_overflow > 0 {
            let _ = writeln!(
                note,
                "> (The rejected-file list is capped at {MAX_REJECTED_FILES}; more may exist.)"
            );
        }
        Some(note)
    }

    /// Plain-text summary for `mesh-mcp doctor`: counts per reason over the
    /// current rejected list, then its first `max_paths` paths.
    pub fn render_summary(&self, max_paths: usize) -> String {
        let mut out = String::new();
        let total = self.rejected.len() + self.rejected_overflow;
        let _ = writeln!(
            out,
            "{} file(s) scanned, {} indexed, {total} not indexed",
            self.files_scanned, self.files_indexed
        );
        for reason in RejectReason::ALL {
            let n = self.rejected.iter().filter(|r| r.reason == reason).count();
            if n > 0 {
                let _ = writeln!(out, "    {}: {n}", reason.label());
            }
        }
        if self.rejected_overflow > 0 {
            // The breakdown above only covers the capped `rejected` list (step 4.1
            // review): say so here, or it reads as a per-reason total that silently
            // falls `rejected_overflow` short of the total line above it.
            let _ = writeln!(
                out,
                "    (+{} more not indexed, beyond the {MAX_REJECTED_FILES}-entry list \
                 and not broken down by reason above)",
                self.rejected_overflow
            );
        }
        for r in self.rejected.iter().take(max_paths) {
            let _ = writeln!(
                out,
                "    - {} ({}, {})",
                r.path.display(),
                r.reason.label(),
                human_size(r.size_bytes)
            );
        }
        if self.rejected.len() > max_paths {
            let _ = writeln!(out, "    - …and {} more", self.rejected.len() - max_paths);
        }
        out
    }

    /// Adds `other`'s counts onto `self` — used to fold one reload pass's
    /// outcomes onto the snapshot's carried-over health.
    pub fn merge(&mut self, other: &IndexHealth) {
        self.files_scanned += other.files_scanned;
        self.files_indexed += other.files_indexed;
        self.files_rejected_oversized += other.files_rejected_oversized;
        self.files_rejected_guard += other.files_rejected_guard;
        self.files_read_error += other.files_read_error;
        self.files_parse_failed += other.files_parse_failed;
    }

    #[inline]
    pub fn total_rejected(&self) -> usize {
        self.files_rejected_oversized + self.files_rejected_guard + self.files_read_error
    }

    /// `false` means at least one file is indexed with facts the indexer
    /// couldn't actually produce (a real parse failure) or couldn't even read.
    /// A file rejected by a size/lexical guard is an intentional, logged
    /// policy decision, not a failure, so it does not affect this.
    #[inline]
    pub fn is_healthy(&self) -> bool {
        self.files_parse_failed == 0 && self.files_read_error == 0
    }
}

/// A path shown inside inline code on one Markdown line: no newline, no backtick.
fn sanitize_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace(['\n', '\r'], " ")
        .replace('`', "'")
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_adds_every_field() {
        let mut a = IndexHealth {
            files_scanned: 10,
            files_indexed: 8,
            files_rejected_oversized: 1,
            files_rejected_guard: 1,
            files_read_error: 0,
            files_parse_failed: 0,
            ..Default::default()
        };
        let b = IndexHealth {
            files_scanned: 3,
            files_indexed: 1,
            files_rejected_oversized: 0,
            files_rejected_guard: 0,
            files_read_error: 1,
            files_parse_failed: 1,
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(a.files_scanned, 13);
        assert_eq!(a.files_indexed, 9);
        assert_eq!(a.files_rejected_oversized, 1);
        assert_eq!(a.files_rejected_guard, 1);
        assert_eq!(a.files_read_error, 1);
        assert_eq!(a.files_parse_failed, 1);
    }

    #[test]
    fn healthy_iff_no_parse_or_read_failures() {
        let mut h = IndexHealth::default();
        assert!(h.is_healthy());
        h.record_oversized();
        h.record_guard_rejected();
        assert!(h.is_healthy(), "guard rejections are policy, not failure");
        h.record_read_error();
        assert!(!h.is_healthy());

        let mut h2 = IndexHealth::default();
        h2.record_parse_failed();
        assert!(!h2.is_healthy());
    }

    fn rej(path: &str, reason: RejectReason, size: u64) -> RejectedFile {
        RejectedFile {
            path: PathBuf::from(path),
            reason,
            size_bytes: size,
        }
    }

    #[test]
    fn rejected_list_is_sorted_deduplicated_and_capped() {
        let mut h = IndexHealth::default();
        h.record_rejected_file("/r/b.ts".into(), RejectReason::Oversized, 1);
        h.record_rejected_file("/r/a.ts".into(), RejectReason::Guard, 2);
        h.record_rejected_file("/r/b.ts".into(), RejectReason::ParseFailed, 3);
        h.normalize_rejected();
        assert_eq!(
            h.rejected,
            vec![
                rej("/r/a.ts", RejectReason::Guard, 2),
                rej("/r/b.ts", RejectReason::ParseFailed, 3)
            ]
        );

        let mut big = IndexHealth::default();
        for i in (0..MAX_REJECTED_FILES + 5).rev() {
            big.record_rejected_file(format!("/r/{i:06}").into(), RejectReason::Oversized, 1);
        }
        big.normalize_rejected();
        assert_eq!(big.rejected.len(), MAX_REJECTED_FILES);
        assert_eq!(big.rejected_overflow, 5);
        assert_eq!(big.rejected[0].path, PathBuf::from("/r/000000"));
    }

    /// Regression (step 4.1 review): `rejected_overflow` must not accumulate
    /// across every reload that ever pushed the list past the cap — the
    /// original `+=` did, since `normalize_rejected` runs on every
    /// `replace_rejected` call (one per incremental reload), drifting upward
    /// without bound over a long-running daemon's life. It must also not fall
    /// back to 0 on the very next incremental reload just because the paths
    /// that overflowed the cap were already truncated off `self.rejected` (and
    /// so invisible to a plain recomputation) — a naive fresh reassignment
    /// does exactly that.
    #[test]
    fn rejected_overflow_reflects_the_current_excess_not_a_running_total() {
        let mut h = IndexHealth::default();
        for i in 0..MAX_REJECTED_FILES + 3 {
            h.record_rejected_file(format!("/r/{i:06}").into(), RejectReason::Oversized, 1);
        }
        h.normalize_rejected();
        assert_eq!(h.rejected_overflow, 3);

        // Further incremental reloads that touch none of the over-cap files (a
        // realistic pass over an unrelated part of the tree) must not add to the
        // overflow already reported: it is still exactly 3 over, not 6, not 9.
        let touched: HashSet<&Path> = HashSet::new();
        for _ in 0..3 {
            h.replace_rejected(&touched, &[]);
            assert_eq!(
                h.rejected_overflow, 3,
                "overflow must not accumulate across repeated reloads"
            );
        }
    }

    #[test]
    fn replace_rejected_drops_fixed_files_and_adds_new_ones() {
        let mut h = IndexHealth::default();
        h.record_rejected_file("/r/fixed.ts".into(), RejectReason::Oversized, 500_000);
        h.record_rejected_file("/r/untouched.ts".into(), RejectReason::Guard, 10);
        h.normalize_rejected();
        let fixed = PathBuf::from("/r/fixed.ts");
        let new = PathBuf::from("/r/new.ts");
        let touched: HashSet<&Path> = [fixed.as_path(), new.as_path()].into_iter().collect();
        let fresh = [rej("/r/new.ts", RejectReason::Oversized, 600_000)];
        assert!(h.replace_rejected(&touched, &fresh));
        let paths: Vec<_> = h.rejected.iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths, vec![new.clone(), PathBuf::from("/r/untouched.ts")]);
        assert!(
            !h.replace_rejected(&touched, &fresh),
            "same outcome: unchanged"
        );
    }

    #[test]
    fn scope_note_filters_and_stays_under_2kb() {
        let mut h = IndexHealth::default();
        for i in 0..50 {
            let long = "x".repeat(120);
            h.record_rejected_file(
                format!("/in/{long}{i:02}.ts").into(),
                RejectReason::Oversized,
                450 * 1024,
            );
        }
        h.record_rejected_file("/out/a.ts".into(), RejectReason::Guard, 5);
        h.normalize_rejected();
        let note = h.scope_note(|p| p.starts_with("/in")).expect("note");
        assert!(note.len() <= NOTE_MAX_BYTES, "{}", note.len());
        assert!(!note.contains("/out/"), "{note}");
        assert!(note.contains("450.0 KB") && note.contains("more"), "{note}");
        assert!(note.contains("read the file directly"), "{note}");
        assert!(h.scope_note(|p| p.starts_with("/none")).is_none());

        let few = h.scope_note(|p| p.starts_with("/out")).expect("note");
        assert!(
            few.contains("`/out/a.ts`") && !few.contains("more"),
            "{few}"
        );
    }
}
