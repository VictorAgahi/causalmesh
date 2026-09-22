use crate::decapitate::LanguageKind;
use std::cell::RefCell;
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
    pub const MAX_FILE_SIZE_BYTES: u64 = 384 * 1024; // 384 KB for standard source code
    pub const MAX_SCHEMA_FILE_SIZE_BYTES: u64 = 1536 * 1024; // 1.5 MB for generated schemas & contracts
    pub const BINARY_SNIFF_LEN: usize = 4096;
    pub const MAX_LINE_LEN_BYTES: usize = 1024;
    pub const MAX_NESTING_DEPTH: usize = 64;
    pub const PARSER_TIMEOUT_MICROS: u64 = 15_000; // 15ms C-FFI timeout
    pub const QUERY_MATCH_LIMIT: u32 = 500;
    pub const MAX_QUERY_STEPS: usize = 10_000; // Anti-ReDoS step limit

    /// Identifies whether a file is a contract definition or generated serialization stub
    pub fn is_contract_or_schema(path: &std::path::Path) -> bool {
        let filename = match path.file_name().and_then(|f| f.to_str()) {
            Some(f) => f.to_ascii_lowercase(),
            None => return false,
        };
        filename.ends_with(".proto")
            || filename.ends_with(".pb.go")
            || filename.ends_with("outerclass.java")
            || filename.ends_with(".pb.ts")
            || filename.ends_with("_pb2.py")
            || filename.ends_with(".pb.rs")
            || filename == "openapi.yaml"
            || filename == "openapi.json"
            || filename == "asyncapi.yaml"
            || filename == "asyncapi.json"
    }

    /// Size budget for `path` (schemas/stubs get 1.5 MB, source files 384 KB).
    #[inline]
    pub fn size_budget(path: &std::path::Path) -> u64 {
        if Self::is_contract_or_schema(path) {
            Self::MAX_SCHEMA_FILE_SIZE_BYTES
        } else {
            Self::MAX_FILE_SIZE_BYTES
        }
    }

    /// Stat-only pre-check, usable *before* reading the file so an oversized
    /// file never costs its full read.
    #[inline]
    pub fn within_size_budget(path: &std::path::Path, metadata: &fs::Metadata) -> bool {
        metadata.len() <= Self::size_budget(path)
    }

    /// Null-byte sniff over the first 4 KB.
    #[inline]
    pub fn looks_binary(content: &[u8]) -> bool {
        let inspect_len = content.len().min(Self::BINARY_SNIFF_LEN);
        content[..inspect_len].contains(&0)
    }

    /// Lexical pre-check with path-aware sizing (1.5 MB budget for schemas/stubs, 384 KB for source files)
    pub fn should_parse_path(
        path: &std::path::Path,
        metadata: &fs::Metadata,
        content: &[u8],
    ) -> bool {
        Self::should_parse_with_budget(metadata, content, Self::size_budget(path))
    }

    /// Default lexical pre-check rejecting oversized, binary, long-line, or deeply nested files
    pub fn should_parse(metadata: &fs::Metadata, content: &[u8]) -> bool {
        Self::should_parse_with_budget(metadata, content, Self::MAX_FILE_SIZE_BYTES)
    }

    /// Lexical pre-check with custom byte size budget
    pub fn should_parse_with_budget(
        metadata: &fs::Metadata,
        content: &[u8],
        max_bytes: u64,
    ) -> bool {
        if metadata.len() > max_bytes {
            tracing::debug!(
                target: "mesh::parser",
                "Rejected file exceeding budget (size: {} bytes, budget: {} bytes)",
                metadata.len(),
                max_bytes
            );
            return false;
        }

        if Self::looks_binary(content) {
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

    /// Fast, context-aware lexical depth scanner ignoring brackets inside comments and string literals
    #[inline]
    pub fn max_nesting_depth(content: &[u8]) -> usize {
        let mut depth = 0usize;
        let mut max_depth = 0usize;
        let mut i = 0;
        let len = content.len();

        while i < len {
            let b = content[i];

            // 1. Single-line comment //
            if b == b'/' && i + 1 < len && content[i + 1] == b'/' {
                i += 2;
                while i < len && content[i] != b'\n' {
                    i += 1;
                }
                continue;
            }

            // 2. Multi-line comment /* ... */
            if b == b'/' && i + 1 < len && content[i + 1] == b'*' {
                i += 2;
                while i + 1 < len && !(content[i] == b'*' && content[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(len);
                continue;
            }

            // 3. Single-line comment # (Python, YAML, Shell)
            if b == b'#' {
                i += 1;
                while i < len && content[i] != b'\n' {
                    i += 1;
                }
                continue;
            }

            // 4. Double quoted string "..."
            if b == b'"' {
                i += 1;
                while i < len {
                    if content[i] == b'\\' {
                        i = (i + 2).min(len);
                    } else if content[i] == b'"' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }

            // 5. Single quoted char or string '...'
            if b == b'\'' {
                i += 1;
                while i < len {
                    if content[i] == b'\\' {
                        i = (i + 2).min(len);
                    } else if content[i] == b'\'' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }

            // 6. Backtick template / raw string `...`
            if b == b'`' {
                i += 1;
                while i < len {
                    if content[i] == b'\\' {
                        i = (i + 2).min(len);
                    } else if content[i] == b'`' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }

            // Syntactic brackets and braces outside literals and comments
            match b {
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
            i += 1;
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
            && Self::create_bounded_parser(&tree_sitter_cpp::LANGUAGE.into()).is_ok()
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

    /// Runs `f` with a thread-local bounded parser for `lang_kind`, creating it on first use.
    ///
    /// `Parser::new` + `set_language` are not free (C-FFI allocation and grammar binding);
    /// recreating one per file on a 20k-file workspace is pure overhead. Rayon workers and
    /// the tokio blocking pool each keep their own instance, so no locking is needed.
    /// Returns `None` for languages without a tree-sitter grammar.
    pub fn with_parser<R>(lang_kind: LanguageKind, f: impl FnOnce(&mut Parser) -> R) -> Option<R> {
        thread_local! {
            static PARSERS: RefCell<[Option<Parser>; LanguageKind::TREE_SITTER_COUNT]> =
                const { RefCell::new([None, None, None, None, None, None]) };
        }

        let slot = lang_kind.tree_sitter_slot()?;
        let language = lang_kind.language()?;

        PARSERS.with(|cell| {
            let mut parsers = cell.borrow_mut();
            let parser = match &mut parsers[slot] {
                Some(p) => p,
                empty => match Self::create_bounded_parser(&language) {
                    Ok(p) => empty.insert(p),
                    Err(e) => {
                        tracing::error!(target: "mesh::parser", "Cannot initialize parser: {e}");
                        return None;
                    }
                },
            };
            let out = f(parser);
            // A timed-out parse leaves the parser mid-state; reset so the next file starts clean.
            parser.reset();
            Some(out)
        })
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

    #[test]
    fn test_nesting_depth_ignores_strings_and_comments() {
        // String literal with 80 brackets should NOT trip the nesting guard
        let code_with_string = br#"
        fn log() {
            let pattern = "[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[";
        }
        "#;
        assert_eq!(AstGuard::max_nesting_depth(code_with_string), 1);

        // Single-line and multi-line comments with deep braces should NOT trip the nesting guard
        let code_with_comments = br#"
        // {{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{
        /* (((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((( */
        # [[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[
        fn clean() {
            return;
        }
        "#;
        assert_eq!(AstGuard::max_nesting_depth(code_with_comments), 1);
    }

    #[test]
    fn test_is_contract_or_schema() {
        assert!(AstGuard::is_contract_or_schema(std::path::Path::new(
            "services/auth.proto"
        )));
        assert!(AstGuard::is_contract_or_schema(std::path::Path::new(
            "client.pb.go"
        )));
        assert!(AstGuard::is_contract_or_schema(std::path::Path::new(
            "AuthOuterClass.java"
        )));
        assert!(AstGuard::is_contract_or_schema(std::path::Path::new(
            "api.pb.ts"
        )));
        assert!(AstGuard::is_contract_or_schema(std::path::Path::new(
            "user_pb2.py"
        )));
        assert!(AstGuard::is_contract_or_schema(std::path::Path::new(
            "contract.pb.rs"
        )));
        assert!(AstGuard::is_contract_or_schema(std::path::Path::new(
            "openapi.yaml"
        )));
        assert!(AstGuard::is_contract_or_schema(std::path::Path::new(
            "asyncapi.json"
        )));

        assert!(!AstGuard::is_contract_or_schema(std::path::Path::new(
            "main.rs"
        )));
        assert!(!AstGuard::is_contract_or_schema(std::path::Path::new(
            "UserController.java"
        )));
        assert!(!AstGuard::is_contract_or_schema(std::path::Path::new(
            "app.ts"
        )));
    }

    #[test]
    fn test_schema_large_budget() {
        let temp = tempfile::tempdir().expect("temp dir");
        let proto_file = temp.path().join("large.proto");
        let rust_file = temp.path().join("large.rs");

        // Write 500 KB file with normal line lengths (exceeds 384 KB MAX_FILE_SIZE, within 1.5 MB MAX_SCHEMA_FILE_SIZE)
        let large_content =
            b"syntax = \"proto3\";\nmessage LargeStub { string id = 1; }\n".repeat(10_000);
        std::fs::write(&proto_file, &large_content).expect("write proto");
        std::fs::write(&rust_file, &large_content).expect("write rust");

        let proto_meta = std::fs::metadata(&proto_file).expect("meta proto");
        let rust_meta = std::fs::metadata(&rust_file).expect("meta rust");

        // Proto file passes under 1.5 MB schema budget
        assert!(AstGuard::should_parse_path(
            &proto_file,
            &proto_meta,
            &large_content
        ));

        // Regular Rust file is rejected by 384 KB budget
        assert!(!AstGuard::should_parse_path(
            &rust_file,
            &rust_meta,
            &large_content
        ));
    }
}
