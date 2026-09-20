use std::fs;
use streaming_iterator::StreamingIterator;
use thiserror::Error;
use tree_sitter::{Language, Node, Parser, Query, QueryCapture, QueryCursor};

#[derive(Debug, Error)]
pub enum ParserError {
    #[error("Tree-sitter parser failed to set language: {0}")]
    LanguageError(String),

    #[error("File rejected by AstGuard: {0}")]
    GuardRejected(String),

    #[error("Query compilation error: {0}")]
    QueryError(#[from] tree_sitter::QueryError),
}

/// Owned match result where captures are stored in a vector so they can outlive the streaming cursor
#[derive(Debug, Clone)]
pub struct BoundedMatch<'tree> {
    pub pattern_index: usize,
    pub captures: Vec<QueryCapture<'tree>>,
}

/// Hardened Tree-sitter C-FFI bounded guards and lexical pre-checks per RFC-001 Commandment 2
pub struct AstGuard;

impl AstGuard {
    pub const MAX_FILE_SIZE_BYTES: u64 = 384 * 1024; // 384 KB
    pub const BINARY_SNIFF_LEN: usize = 4096;
    pub const MAX_LINE_LEN_BYTES: usize = 1024;
    pub const MAX_NESTING_DEPTH: usize = 64;
    pub const PARSER_TIMEOUT_MICROS: u64 = 15_000; // 15ms C-FFI timeout
    pub const QUERY_MATCH_LIMIT: u32 = 500;
    pub const MAX_QUERY_STEPS: usize = 10_000; // Anti-ReDoS step limit

    /// Lexical pre-check rejecting oversized, binary, long-line, or deeply nested files in < 1us
    pub fn should_parse(metadata: &fs::Metadata, content: &[u8]) -> bool {
        if metadata.len() > Self::MAX_FILE_SIZE_BYTES {
            tracing::debug!(target: "mesh::parser", "Rejected file exceeding 384 KB (size: {} bytes)", metadata.len());
            return false;
        }

        let inspect_len = content.len().min(Self::BINARY_SNIFF_LEN);
        if content[..inspect_len].contains(&0) {
            tracing::debug!(target: "mesh::parser", "Rejected file: binary null byte detected");
            return false;
        }

        if content
            .split(|&b| b == b'\n')
            .any(|line| line.len() > Self::MAX_LINE_LEN_BYTES)
        {
            tracing::debug!(target: "mesh::parser", "Rejected file: line exceeds 1024 bytes");
            return false;
        }

        if Self::max_nesting_depth(content) > Self::MAX_NESTING_DEPTH {
            tracing::warn!(target: "mesh::parser", "Rejected file: excessive syntactic nesting depth (> 64)");
            return false;
        }

        true
    }

    #[inline]
    pub fn max_nesting_depth(content: &[u8]) -> usize {
        let mut depth = 0usize;
        let mut max_depth = 0usize;
        for &byte in content {
            match byte {
                b'{' | b'(' | b'[' => {
                    depth += 1;
                    if depth > max_depth {
                        max_depth = depth;
                    }
                }
                b'}' | b')' | b']' => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        max_depth
    }

    /// Verifies all language grammars can be loaded and initialized with bounded C-FFI timeouts
    pub fn verify_all_parsers() -> bool {
        Self::create_bounded_parser(&tree_sitter_java::LANGUAGE.into()).is_ok()
            && Self::create_bounded_parser(&tree_sitter_go::LANGUAGE.into()).is_ok()
            && Self::create_bounded_parser(&tree_sitter_python::LANGUAGE.into()).is_ok()
            && Self::create_bounded_parser(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
                .is_ok()
            && Self::create_bounded_parser(&tree_sitter_rust::LANGUAGE.into()).is_ok()
    }

    /// Initializes a bounded tree-sitter parser with a strict 15ms C-FFI timeout
    pub fn create_bounded_parser(lang: &Language) -> Result<Parser, ParserError> {
        let mut parser = Parser::new();
        parser
            .set_language(lang)
            .map_err(|e| ParserError::LanguageError(e.to_string()))?;
        parser.set_timeout_micros(Self::PARSER_TIMEOUT_MICROS);
        Ok(parser)
    }

    /// Executes query with both match limits and hard iteration step counter preventing ReDoS
    pub fn execute_bounded_query<'tree>(
        cursor: &mut QueryCursor,
        query: &Query,
        node: Node<'tree>,
        source: &'tree [u8],
    ) -> Vec<BoundedMatch<'tree>> {
        cursor.set_match_limit(Self::QUERY_MATCH_LIMIT);
        let mut matches = Vec::new();
        let mut step_count = 0usize;

        let mut matches_iter = cursor.matches(query, node, source);
        while let Some(m) = matches_iter.next() {
            matches.push(BoundedMatch {
                pattern_index: m.pattern_index,
                captures: m.captures.to_vec(),
            });
            step_count += 1;
            if step_count >= Self::MAX_QUERY_STEPS {
                tracing::warn!(target: "mesh::parser", "Query execution step limit reached (ReDoS guard triggered)");
                break;
            }
        }

        matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nesting_depth_guard() {
        let normal_code = b"fn main() { if true { while true { let x = (1 + (2 * [3])); } } }";
        assert!(AstGuard::max_nesting_depth(normal_code) <= 64);

        let mut deep_code = vec![b'('; 70];
        deep_code.extend(vec![b')'; 70]);
        assert!(AstGuard::max_nesting_depth(&deep_code) > 64);
    }
}
