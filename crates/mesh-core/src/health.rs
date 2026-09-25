use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
        };
        let b = IndexHealth {
            files_scanned: 3,
            files_indexed: 1,
            files_rejected_oversized: 0,
            files_rejected_guard: 0,
            files_read_error: 1,
            files_parse_failed: 1,
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
}
