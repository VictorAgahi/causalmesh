//! Result cache for `smart_search`, keyed by the full request shape and scoped to
//! one snapshot generation.
//!
//! A cached page is only ever served against the exact snapshot generation it was
//! computed from: every `AppState::install_snapshot` bumps the generation, and the
//! first lookup or insert that sees a newer generation drops every entry. A result
//! computed from an older snapshot that races a reload is discarded on insert
//! instead of resurrecting stale data.

use crate::types::CompactStr;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Everything that changes a `smart_search` answer for a fixed snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchCacheKey {
    pub query: CompactStr,
    /// Canonical `ValidatedScope` path, so `./a` and `a` share one entry.
    pub scope: PathBuf,
    pub include_body: bool,
    pub fuzzy: bool,
    pub limit: u32,
    pub offset: u32,
}

/// One rendered result page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedSearch {
    pub text: String,
    pub files_accessed: Vec<String>,
}

#[derive(Debug, Default)]
struct Inner {
    generation: u64,
    entries: HashMap<SearchCacheKey, Arc<CachedSearch>>,
}

/// Bounded, generation-invalidated `smart_search` result cache.
#[derive(Debug, Default)]
pub struct SearchCache {
    inner: Mutex<Inner>,
}

impl SearchCache {
    /// Entry bound: past it the map is cleared rather than growing without limit
    /// (a rendered page is at most 48 KB, so this caps the cache near 12 MB).
    pub const MAX_ENTRIES: usize = 256;

    /// The page cached for `key` at exactly `generation`, if any.
    pub fn get(&self, generation: u64, key: &SearchCacheKey) -> Option<Arc<CachedSearch>> {
        let mut inner = self.inner.lock().ok()?;
        if inner.generation != generation {
            if generation > inner.generation {
                inner.entries.clear();
                inner.generation = generation;
            }
            return None;
        }
        inner.entries.get(key).cloned()
    }

    /// Stores a page computed from the snapshot at `generation`. Dropped when a
    /// newer generation has already been observed (the page is stale).
    pub fn insert(&self, generation: u64, key: SearchCacheKey, value: CachedSearch) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if generation < inner.generation {
            return;
        }
        if generation > inner.generation {
            inner.entries.clear();
            inner.generation = generation;
        }
        if inner.entries.len() >= Self::MAX_ENTRIES && !inner.entries.contains_key(&key) {
            inner.entries.clear();
        }
        inner.entries.insert(key, Arc::new(value));
    }

    /// Number of live entries (diagnostics and tests).
    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.entries.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(q: &str) -> SearchCacheKey {
        SearchCacheKey {
            query: q.into(),
            scope: PathBuf::from("/ws"),
            include_body: false,
            fuzzy: false,
            limit: 20,
            offset: 0,
        }
    }

    fn page(t: &str) -> CachedSearch {
        CachedSearch {
            text: t.to_string(),
            files_accessed: vec![],
        }
    }

    #[test]
    fn hit_only_at_same_generation() {
        let cache = SearchCache::default();
        cache.insert(3, key("A"), page("a"));
        assert_eq!(
            cache.get(3, &key("A")).map(|p| p.text.clone()),
            Some("a".into())
        );
        assert!(cache.get(3, &key("B")).is_none());
        // A generation bump invalidates everything.
        assert!(cache.get(4, &key("A")).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn stale_insert_after_bump_is_dropped() {
        let cache = SearchCache::default();
        assert!(cache.get(5, &key("A")).is_none());
        // Computed from generation 4 while a reload already installed 5.
        cache.insert(4, key("A"), page("stale"));
        assert!(cache.get(5, &key("A")).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn bounded() {
        let cache = SearchCache::default();
        for i in 0..(SearchCache::MAX_ENTRIES + 10) {
            cache.insert(1, key(&format!("q{i}")), page("x"));
        }
        assert!(cache.len() <= SearchCache::MAX_ENTRIES);
    }
}
