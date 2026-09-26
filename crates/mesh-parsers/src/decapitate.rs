use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageKind {
    Java,
    Go,
    Python,
    TypeScript,
    Rust,
    Cpp,
    Kotlin,
    CSharp,
    Ruby,
    Php,
    Swift,
    Scala,
    Protobuf,
    Yaml,
    Unknown,
}

impl LanguageKind {
    /// Number of variants backed by a tree-sitter grammar (Java, Go, Python, TypeScript, Rust,
    /// Cpp, Kotlin, CSharp, Ruby, Php, Swift, Scala, Protobuf).
    pub const TREE_SITTER_COUNT: usize = 13;

    /// Lowercase name, allocation-free (was `format!("{:?}").to_lowercase()` per file).
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Go => "go",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Rust => "rust",
            Self::Cpp => "cpp",
            Self::Kotlin => "kotlin",
            Self::CSharp => "csharp",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Swift => "swift",
            Self::Scala => "scala",
            Self::Protobuf => "protobuf",
            Self::Yaml => "yaml",
            Self::Unknown => "unknown",
        }
    }

    /// Slot index for the thread-local parser cache; `None` for non tree-sitter languages.
    #[inline]
    pub(crate) fn tree_sitter_slot(self) -> Option<usize> {
        match self {
            Self::Java => Some(0),
            Self::Go => Some(1),
            Self::Python => Some(2),
            Self::TypeScript => Some(3),
            Self::Rust => Some(4),
            Self::Cpp => Some(5),
            Self::Kotlin => Some(6),
            Self::CSharp => Some(7),
            Self::Ruby => Some(8),
            Self::Php => Some(9),
            Self::Swift => Some(10),
            Self::Scala => Some(11),
            Self::Protobuf => Some(12),
            Self::Yaml | Self::Unknown => None,
        }
    }

    /// Tree-sitter grammar for this language, if any.
    pub fn language(self) -> Option<tree_sitter::Language> {
        match self {
            Self::Java => Some(tree_sitter_java::LANGUAGE.into()),
            Self::Go => Some(tree_sitter_go::LANGUAGE.into()),
            Self::Python => Some(tree_sitter_python::LANGUAGE.into()),
            Self::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            Self::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            Self::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
            Self::Kotlin => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
            Self::CSharp => Some(tree_sitter_c_sharp::language()),
            Self::Ruby => Some(tree_sitter_ruby::LANGUAGE.into()),
            Self::Php => Some(tree_sitter_php::LANGUAGE_PHP.into()),
            Self::Swift => Some(tree_sitter_swift::LANGUAGE.into()),
            Self::Scala => Some(tree_sitter_scala::LANGUAGE.into()),
            Self::Protobuf => Some(tree_sitter_proto::LANGUAGE.into()),
            Self::Yaml | Self::Unknown => None,
        }
    }

    pub fn from_path(path_str: &str) -> Self {
        if path_str.ends_with(".proto") {
            Self::Protobuf
        } else if path_str.ends_with(".java") {
            Self::Java
        } else if path_str.ends_with(".go") {
            Self::Go
        } else if path_str.ends_with(".py") {
            Self::Python
        } else if path_str.ends_with(".ts")
            || path_str.ends_with(".tsx")
            || path_str.ends_with(".js")
        {
            Self::TypeScript
        } else if path_str.ends_with(".rs") {
            Self::Rust
        } else if path_str.ends_with(".kt") || path_str.ends_with(".kts") {
            Self::Kotlin
        } else if path_str.ends_with(".cs") {
            Self::CSharp
        } else if path_str.ends_with(".rb") || path_str.ends_with(".rake") {
            Self::Ruby
        } else if path_str.ends_with(".php") || path_str.ends_with(".phtml") {
            Self::Php
        } else if path_str.ends_with(".swift") {
            Self::Swift
        } else if path_str.ends_with(".scala") || path_str.ends_with(".sc") {
            Self::Scala
        } else if path_str.ends_with(".cpp")
            || path_str.ends_with(".cc")
            || path_str.ends_with(".cxx")
            || path_str.ends_with(".hpp")
            || path_str.ends_with(".hh")
            || path_str.ends_with(".hxx")
            || path_str.ends_with(".h")
        {
            Self::Cpp
        } else if path_str.ends_with(".yaml") || path_str.ends_with(".yml") {
            Self::Yaml
        } else {
            Self::Unknown
        }
    }
}

/// Decapitated text plus its mapping back onto the original source.
#[derive(Debug, Clone)]
pub struct DecapitatedSource {
    pub text: String,
    /// One entry per `text.lines()` line: the `(first, last)` 1-based original
    /// lines it was produced from. `first == last` for an untouched line; a line
    /// holding a stripped body spans the body's whole original extent.
    pub line_map: Vec<(u32, u32)>,
}

fn identity_line_map(content: &str) -> Vec<(u32, u32)> {
    (1..=content.lines().count() as u32)
        .map(|l| (l, l))
        .collect()
}

fn count_newlines(s: &str) -> u32 {
    s.bytes().filter(|b| *b == b'\n').count() as u32
}

/// Accumulates the original-line span of the output line being built.
#[derive(Default)]
struct LineTracker {
    current: Option<(u32, u32)>,
}

impl LineTracker {
    fn push(&mut self, byte: u8, orig_line: u32, map: &mut Vec<(u32, u32)>) {
        let span = self.current.get_or_insert((orig_line, orig_line));
        span.1 = orig_line;
        if byte == b'\n' {
            map.push(*span);
            self.current = None;
        }
    }

    /// Flushes a final line with no trailing newline (`str::lines` yields it too).
    fn finish(self, map: &mut Vec<(u32, u32)>) {
        if let Some(span) = self.current {
            map.push(span);
        }
    }
}

/// Polyglot AST Decapitation Engine per RFC-001 Section 4.4
pub struct AstDecapitator;

impl AstDecapitator {
    /// Compact bounded stub (122 bytes <= 256 bytes) protecting LLM context on parser timeout or resource exhaustion
    pub const BOUNDED_ERROR_STUB: &'static str =
        "// [MeshMCP Warning: AST decapitation bounded or parser timeout (>15ms). Body stripped to protect context window.]\n";

    /// Decapitates imperative method bodies using the appropriate language parser automatically
    pub fn decapitate_auto(content: &str, lang_kind: LanguageKind, include_body: bool) -> String {
        if include_body {
            return content.to_string();
        }

        match lang_kind {
            LanguageKind::Protobuf | LanguageKind::Yaml => return content.to_string(),
            LanguageKind::Unknown => {
                return if content.len() > 1024 {
                    Self::BOUNDED_ERROR_STUB.to_string()
                } else {
                    content.to_string()
                };
            }
            _ => {}
        }

        crate::guard::AstGuard::with_parser(lang_kind, |parser| {
            Self::decapitate(content, lang_kind, parser, false)
        })
        .unwrap_or_else(|| Self::BOUNDED_ERROR_STUB.to_string())
    }

    /// Decapitates imperative method bodies while preserving docstrings, annotations, and contracts.
    pub fn decapitate(
        content: &str,
        lang_kind: LanguageKind,
        parser: &mut Parser,
        include_body: bool,
    ) -> String {
        if include_body {
            return content.to_string();
        }

        let tree = match parser.parse(content, None) {
            Some(t) => t,
            None => {
                // Commandment 2 & RFC-001: NEVER return the raw gigantic file on timeout or failure.
                // Return bounded error stub <= 256 bytes protecting LLM context window.
                tracing::warn!(
                    target: "mesh::parser",
                    "Parser timeout (>15ms) or C-FFI failure; returning bounded stub (<= 256 bytes)"
                );
                return Self::BOUNDED_ERROR_STUB.to_string();
            }
        };

        let mut replacements: Vec<(usize, usize, std::borrow::Cow<'static, str>)> = Vec::new();
        Self::collect_body_replacements(content, tree.root_node(), lang_kind, &mut replacements, 0);
        Self::apply_replacements(content, replacements, None)
    }

    /// [`Self::decapitate_auto`] plus, for every output line, the span of
    /// original 1-based lines it came from — see [`DecapitatedSource::line_map`].
    /// The map is derived from the replaced tree-sitter nodes' byte ranges, so a
    /// caller can anchor a snippet on an exact original line instead of guessing
    /// by re-matching decapitated text against the source.
    pub fn decapitate_auto_mapped(
        content: &str,
        lang_kind: LanguageKind,
        include_body: bool,
    ) -> DecapitatedSource {
        let identity = || DecapitatedSource {
            text: content.to_string(),
            line_map: identity_line_map(content),
        };
        let stub = || DecapitatedSource {
            text: Self::BOUNDED_ERROR_STUB.to_string(),
            line_map: vec![(1, content.lines().count().max(1) as u32)],
        };
        if include_body {
            return identity();
        }
        match lang_kind {
            LanguageKind::Protobuf | LanguageKind::Yaml => return identity(),
            LanguageKind::Unknown => {
                return if content.len() > 1024 {
                    stub()
                } else {
                    identity()
                };
            }
            _ => {}
        }

        crate::guard::AstGuard::with_parser(lang_kind, |parser| {
            let Some(tree) = parser.parse(content, None) else {
                tracing::warn!(
                    target: "mesh::parser",
                    "Parser timeout or C-FFI failure; returning bounded stub (<= 256 bytes)"
                );
                return None;
            };
            let mut replacements = Vec::new();
            Self::collect_body_replacements(
                content,
                tree.root_node(),
                lang_kind,
                &mut replacements,
                0,
            );
            let mut line_map = Vec::new();
            let text = Self::apply_replacements(content, replacements, Some(&mut line_map));
            Some(DecapitatedSource { text, line_map })
        })
        .flatten()
        .unwrap_or_else(stub)
    }

    /// Applies non-overlapping `(start, end, text)` byte-range replacements in one
    /// forward pass. A replacement overlapping an already-applied one (or not on a
    /// char boundary) is skipped rather than producing a corrupt splice. When
    /// `line_map` is given it receives one `(first, last)` original-line span per
    /// output line: copied bytes keep their own line; a replacement's first byte
    /// maps to the line its node starts on and the rest to the line it ends on,
    /// so `fn f() { /* stripped */ }` spans the function's full original extent.
    fn apply_replacements(
        content: &str,
        mut replacements: Vec<(usize, usize, std::borrow::Cow<'static, str>)>,
        mut line_map: Option<&mut Vec<(u32, u32)>>,
    ) -> String {
        if replacements.is_empty() {
            if let Some(map) = line_map {
                *map = identity_line_map(content);
            }
            return content.to_string();
        }
        // Ascending start; at an identical start an insertion (`start == end`, e.g. a
        // synthetic TS return type) precedes the range replaced after it.
        replacements.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let mut out = String::with_capacity(content.len());
        let mut tracker = LineTracker::default();
        let mut cursor = 0usize;
        let mut orig_line: u32 = 1;
        for (start, end, replacement) in replacements {
            if start < cursor
                || end > content.len()
                || start > end
                || !content.is_char_boundary(start)
                || !content.is_char_boundary(end)
            {
                continue;
            }
            let kept = &content[cursor..start];
            out.push_str(kept);
            if let Some(map) = line_map.as_deref_mut() {
                for b in kept.bytes() {
                    tracker.push(b, orig_line, map);
                    if b == b'\n' {
                        orig_line += 1;
                    }
                }
            } else {
                orig_line += count_newlines(kept);
            }
            let start_line = orig_line;
            orig_line += count_newlines(&content[start..end]);
            out.push_str(&replacement);
            if let Some(map) = line_map.as_deref_mut() {
                for (i, b) in replacement.bytes().enumerate() {
                    tracker.push(b, if i == 0 { start_line } else { orig_line }, map);
                }
            }
            cursor = end;
        }
        let rest = &content[cursor..];
        out.push_str(rest);
        if let Some(map) = line_map {
            for b in rest.bytes() {
                tracker.push(b, orig_line, map);
                if b == b'\n' {
                    orig_line += 1;
                }
            }
            tracker.finish(map);
        }
        out
    }

    fn collect_body_replacements(
        source: &str,
        node: Node,
        lang_kind: LanguageKind,
        replacements: &mut Vec<(usize, usize, std::borrow::Cow<'static, str>)>,
        depth: usize,
    ) {
        // Guards against a native stack overflow: `AstGuard::max_nesting_depth` is a
        // lexical bracket count over raw bytes and only a rough proxy for real CST
        // depth (chained calls/generics/match arms add tree-sitter nesting without
        // brackets), so a file can pass that pre-parse filter and still produce a
        // parse tree deep enough to blow the stack here. Cap at the same limit for
        // consistency and leave unvisited subtrees unmodified (safer than emitting a
        // partial/incorrect replacement span).
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

        let kind = node.kind();

        match lang_kind {
            LanguageKind::Java
                if kind == "method_declaration" || kind == "constructor_declaration" =>
            {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((
                        body.start_byte(),
                        body.end_byte(),
                        std::borrow::Cow::Borrowed("{ /* stripped */ }"),
                    ));
                    return;
                }
            }
            LanguageKind::Go if kind == "function_declaration" || kind == "method_declaration" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((
                        body.start_byte(),
                        body.end_byte(),
                        std::borrow::Cow::Borrowed("{ /* stripped */ }"),
                    ));
                    return;
                }
            }
            LanguageKind::TypeScript
                if kind == "method_definition"
                    || kind == "function_declaration"
                    || kind == "function_item"
                    || kind == "arrow_function" =>
            {
                if let Some(body) = node.child_by_field_name("body") {
                    // Check if explicit return type is absent; if so, attempt synthetic inference
                    if node.child_by_field_name("return_type").is_none() {
                        let keys = Self::extract_returned_object_keys(source, body);
                        if !keys.is_empty() {
                            let insert_pos = node
                                .child_by_field_name("parameters")
                                .or_else(|| node.child_by_field_name("parameter"))
                                .map(|p| p.end_byte());

                            if let Some(pos) = insert_pos {
                                let fields = keys
                                    .iter()
                                    .map(|k| format!("{}: any", k))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                let synthetic_type = format!(": {{ {} }}", fields);
                                replacements.push((
                                    pos,
                                    pos,
                                    std::borrow::Cow::Owned(synthetic_type),
                                ));
                            }
                        }
                    }

                    if body.kind() == "statement_block" {
                        replacements.push((
                            body.start_byte(),
                            body.end_byte(),
                            std::borrow::Cow::Borrowed("{ /* stripped */ }"),
                        ));
                        return;
                    } else if kind == "arrow_function" {
                        replacements.push((
                            body.start_byte(),
                            body.end_byte(),
                            std::borrow::Cow::Borrowed("/* stripped */"),
                        ));
                        return;
                    }
                }
            }
            LanguageKind::Rust if kind == "function_item" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((
                        body.start_byte(),
                        body.end_byte(),
                        std::borrow::Cow::Borrowed("{ /* stripped */ }"),
                    ));
                    return;
                }
            }
            LanguageKind::Cpp if kind == "function_definition" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((
                        body.start_byte(),
                        body.end_byte(),
                        std::borrow::Cow::Borrowed("{ /* stripped */ }"),
                    ));
                    return;
                }
            }
            LanguageKind::Python if kind == "function_definition" => {
                if let Some(body) = node.child_by_field_name("body") {
                    // The docstring is the only contract a partially-typed Python
                    // function has, so it survives; only the statements after it go.
                    match Self::python_docstring(body) {
                        Some(doc) if doc.end_byte() < body.end_byte() => {
                            let tail = if doc.start_position().row == node.start_position().row {
                                // `def f(): """doc"""; return 1` — stay on one line.
                                std::borrow::Cow::Borrowed("; ...")
                            } else {
                                let indent = " ".repeat(doc.start_position().column);
                                std::borrow::Cow::Owned(format!("\n{indent}..."))
                            };
                            replacements.push((doc.end_byte(), body.end_byte(), tail));
                        }
                        // Docstring-only body: nothing to strip.
                        Some(_) => {}
                        None => replacements.push((
                            body.start_byte(),
                            body.end_byte(),
                            std::borrow::Cow::Borrowed(" ..."),
                        )),
                    }
                    return;
                }
            }
            LanguageKind::CSharp
                if kind == "method_declaration" || kind == "constructor_declaration" =>
            {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((
                        body.start_byte(),
                        body.end_byte(),
                        std::borrow::Cow::Borrowed("{ /* stripped */ }"),
                    ));
                    return;
                }
            }
            LanguageKind::Kotlin if kind == "function_declaration" => {
                let mut cursor = node.walk();
                let mut body = None;
                for child in node.children(&mut cursor) {
                    if child.kind() == "function_body" {
                        body = Some(child);
                        break;
                    }
                }
                if let Some(body) = body {
                    // `function_body`'s span includes the leading `=` for expression
                    // bodies (`fun f() = expr`); block bodies (`{ ... }`) keep their
                    // braces, expression bodies keep the `=` but drop the expression,
                    // mirroring the TypeScript concise-arrow-function rule.
                    let is_block = source
                        .as_bytes()
                        .get(body.start_byte())
                        .is_some_and(|b| *b == b'{');
                    let replacement = if is_block {
                        std::borrow::Cow::Borrowed("{ /* stripped */ }")
                    } else {
                        std::borrow::Cow::Borrowed("= /* stripped */")
                    };
                    replacements.push((body.start_byte(), body.end_byte(), replacement));
                    return;
                }
            }
            LanguageKind::Ruby if kind == "method" || kind == "singleton_method" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((
                        body.start_byte(),
                        body.end_byte(),
                        std::borrow::Cow::Borrowed("\n  # stripped\n"),
                    ));
                    return;
                }
            }
            LanguageKind::Php if kind == "method_declaration" || kind == "function_definition" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((
                        body.start_byte(),
                        body.end_byte(),
                        std::borrow::Cow::Borrowed("{ /* stripped */ }"),
                    ));
                    return;
                }
            }
            LanguageKind::Swift if kind == "function_declaration" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((
                        body.start_byte(),
                        body.end_byte(),
                        std::borrow::Cow::Borrowed("{ /* stripped */ }"),
                    ));
                    return;
                }
            }
            LanguageKind::Scala if kind == "function_definition" => {
                if let Some(body) = node.child_by_field_name("body") {
                    let replacement = if body.kind() == "block" {
                        std::borrow::Cow::Borrowed("{ /* stripped */ }")
                    } else {
                        std::borrow::Cow::Borrowed("/* stripped */")
                    };
                    replacements.push((body.start_byte(), body.end_byte(), replacement));
                    return;
                }
            }
            _ => {}
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_body_replacements(source, child, lang_kind, replacements, depth + 1);
        }
    }

    /// Recursively collects key names from returned object literals to synthesize inferred return types
    /// The docstring of a Python `body: block`: its first statement (comments are
    /// skipped — tree-sitter keeps them as named children) when that statement is
    /// a bare string literal expression.
    fn python_docstring(body: Node) -> Option<Node> {
        let mut cursor = body.walk();
        let first = body
            .named_children(&mut cursor)
            .find(|c| c.kind() != "comment")?;
        if first.kind() != "expression_statement" {
            return None;
        }
        let expr = first.named_child(0)?;
        matches!(expr.kind(), "string" | "concatenated_string").then_some(first)
    }

    fn extract_returned_object_keys(source: &str, node: Node) -> Vec<String> {
        let mut keys = Vec::new();
        Self::collect_keys_recursive(source, node, &mut keys, 0);
        keys.sort();
        keys.dedup();
        keys
    }

    fn collect_keys_recursive(source: &str, node: Node, keys: &mut Vec<String>, depth: usize) {
        if depth > 8 {
            return;
        }
        let kind = node.kind();
        match kind {
            "object" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "pair" {
                        if let Some(key) = child.child_by_field_name("key") {
                            if key.start_byte() < source.len() && key.end_byte() <= source.len() {
                                let key_text = source[key.start_byte()..key.end_byte()].trim();
                                if !key_text.is_empty()
                                    && key_text
                                        .chars()
                                        .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                                {
                                    keys.push(key_text.to_string());
                                }
                            }
                        }
                    } else if (child.kind() == "shorthand_property_identifier"
                        || child.kind() == "shorthand_property_identifier_pattern")
                        && child.start_byte() < source.len()
                        && child.end_byte() <= source.len()
                    {
                        let key_text = source[child.start_byte()..child.end_byte()].trim();
                        if !key_text.is_empty()
                            && key_text
                                .chars()
                                .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                        {
                            keys.push(key_text.to_string());
                        }
                    }
                }
            }
            "parenthesized_expression" | "as_expression" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    Self::collect_keys_recursive(source, child, keys, depth + 1);
                }
            }
            "statement_block" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "return_statement" {
                        if let Some(val) = child.named_child(0) {
                            Self::collect_keys_recursive(source, val, keys, depth + 1);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::AstGuard;
    use tree_sitter::Parser;

    #[test]
    fn test_rust_decapitation() {
        let code = r#"
/// Contract docstring to keep
fn authenticate_user(id: u64) -> bool {
    let mut x = 1;
    for _ in 0..100 {
        x += 1;
    }
    x > 50
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        let decapitated = AstDecapitator::decapitate(code, LanguageKind::Rust, &mut parser, false);
        assert!(decapitated.contains("/// Contract docstring to keep"));
        assert!(decapitated.contains("fn authenticate_user(id: u64) -> bool { /* stripped */ }"));
        assert!(!decapitated.contains("x > 50"));
    }

    #[test]
    fn test_cpp_decapitation() {
        let code = r#"
/// Keep this doc comment
bool AuthService::Authenticate(const std::string& token) {
    if (token.empty()) {
        return false;
    }
    return validate(token);
}
"#;
        let decapitated = AstDecapitator::decapitate_auto(code, LanguageKind::Cpp, false);
        assert!(decapitated.contains("/// Keep this doc comment"));
        assert!(decapitated.contains(
            "bool AuthService::Authenticate(const std::string& token) { /* stripped */ }"
        ));
        assert!(!decapitated.contains("validate(token)"));
        assert_eq!(LanguageKind::from_path("include/auth.h"), LanguageKind::Cpp);
        assert_eq!(LanguageKind::from_path("src/auth.cc"), LanguageKind::Cpp);
        assert_eq!(LanguageKind::Cpp.as_str(), "cpp");
    }

    #[test]
    fn test_typescript_decapitation() {
        let code = r#"
export class AuthController {
    @GrpcMethod('AuthService', 'Login')
    async login(req: Request): Promise<Response> {
        const token = jwt.sign(req.user);
        return { token };
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();

        let decapitated =
            AstDecapitator::decapitate(code, LanguageKind::TypeScript, &mut parser, false);
        assert!(decapitated.contains("@GrpcMethod('AuthService', 'Login')"));
        assert!(
            decapitated.contains("async login(req: Request): Promise<Response> { /* stripped */ }")
        );
        assert!(!decapitated.contains("jwt.sign"));
    }

    #[test]
    fn test_java_decapitation() {
        let code = r#"
@RestController
public class AuthController {
    @PostMapping("/login")
    public ResponseEntity<TokenResponse> login(@RequestBody LoginRequest req) {
        String token = authService.generate(req);
        return ResponseEntity.ok(new TokenResponse(token));
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        let decapitated = AstDecapitator::decapitate(code, LanguageKind::Java, &mut parser, false);
        assert!(decapitated.contains("@RestController"));
        assert!(decapitated.contains("@PostMapping(\"/login\")"));
        assert!(decapitated.contains("public ResponseEntity<TokenResponse> login(@RequestBody LoginRequest req) { /* stripped */ }"));
        assert!(!decapitated.contains("authService.generate"));
    }

    #[test]
    fn test_go_decapitation() {
        let code = r#"
package auth

func (s *AuthServer) Authenticate(ctx context.Context, req *AuthRequest) (*AuthResponse, error) {
    token, err := s.jwt.Sign(req.UserId)
    if err != nil {
        return nil, err
    }
    return &AuthResponse{Token: token}, nil
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_go::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        let decapitated = AstDecapitator::decapitate(code, LanguageKind::Go, &mut parser, false);
        assert!(decapitated.contains("func (s *AuthServer) Authenticate(ctx context.Context, req *AuthRequest) (*AuthResponse, error) { /* stripped */ }"));
        assert!(!decapitated.contains("s.jwt.Sign"));
    }

    #[test]
    fn test_python_decapitation() {
        let code = r#"
class AuthService:
    @tracer.trace("login")
    def login(self, username: str, secret: str) -> dict:
        token = generate_jwt(username)
        return {"token": token}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_python::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        let decapitated =
            AstDecapitator::decapitate(code, LanguageKind::Python, &mut parser, false);
        assert!(decapitated.contains("@tracer.trace(\"login\")"));
        assert!(decapitated.contains("def login(self, username: str, secret: str) -> dict:"));
        assert!(decapitated.contains("..."));
        assert!(!decapitated.contains("generate_jwt"));
    }

    /// A Python docstring is parsed as the first `expression_statement` of the
    /// body; replacing the whole body with `...` used to destroy it, and with it
    /// the only contract a partially-typed function carries.
    #[test]
    fn test_python_docstring_preserved() {
        let code = r#"
class AuthService:
    def login(self, username, secret):
        # leading comment
        """Authenticate a user.

        Returns a dict with a signed `token`.
        """
        token = generate_jwt(username)
        return {"token": token}

    def ping(self):
        """Only a docstring."""

    def short(self): """One-liner."""; return compute()

    def plain(self):
        return helper()
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_python::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        let out = AstDecapitator::decapitate(code, LanguageKind::Python, &mut parser, false);
        assert!(out.contains("\"\"\"Authenticate a user.\n\n        Returns a dict with a signed `token`.\n        \"\"\"\n        ..."), "{out}");
        assert!(!out.contains("generate_jwt"), "{out}");
        assert!(
            out.contains("def ping(self):\n        \"\"\"Only a docstring.\"\"\""),
            "{out}"
        );
        assert!(
            out.contains("def short(self): \"\"\"One-liner.\"\"\"; ..."),
            "{out}"
        );
        assert!(!out.contains("compute()"), "{out}");
        assert!(out.contains("def plain(self):\n         ..."), "{out}");
        assert!(!out.contains("helper()"), "{out}");
    }

    #[test]
    fn test_kotlin_decapitation() {
        let code = r#"
@RestController
class AuthController {
    @PostMapping("/login")
    fun login(req: LoginRequest): TokenResponse {
        val token = authService.generate(req)
        return TokenResponse(token)
    }

    fun shortcut(): Int = 42
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_kotlin_ng::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let decapitated =
            AstDecapitator::decapitate(code, LanguageKind::Kotlin, &mut parser, false);
        assert!(decapitated.contains("@RestController"));
        assert!(decapitated.contains("@PostMapping(\"/login\")"));
        assert!(
            decapitated.contains("fun login(req: LoginRequest): TokenResponse { /* stripped */ }")
        );
        assert!(!decapitated.contains("authService.generate"));
        assert!(decapitated.contains("fun shortcut(): Int = /* stripped */"));
        assert_eq!(LanguageKind::from_path("Foo.kt"), LanguageKind::Kotlin);
        assert_eq!(
            LanguageKind::from_path("build.gradle.kts"),
            LanguageKind::Kotlin
        );
        assert_eq!(LanguageKind::Kotlin.as_str(), "kotlin");
    }

    #[test]
    fn test_csharp_decapitation() {
        let code = r#"
[ApiController]
public class AuthController : ControllerBase
{
    [HttpPost("/login")]
    public TokenResponse Login(LoginRequest req)
    {
        var token = authService.Generate(req);
        return new TokenResponse(token);
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_c_sharp::language();
        parser.set_language(&lang).unwrap();
        let decapitated =
            AstDecapitator::decapitate(code, LanguageKind::CSharp, &mut parser, false);
        assert!(decapitated.contains("[ApiController]"));
        assert!(decapitated.contains("[HttpPost(\"/login\")]"));
        assert!(decapitated.contains("public TokenResponse Login(LoginRequest req)"));
        assert!(decapitated.contains("{ /* stripped */ }"));
        assert!(!decapitated.contains("authService.Generate"));
        assert_eq!(LanguageKind::from_path("Auth.cs"), LanguageKind::CSharp);
        assert_eq!(LanguageKind::CSharp.as_str(), "csharp");
    }

    #[test]
    fn test_ruby_decapitation() {
        let code = r#"
class AuthService
  # Contract docstring to keep
  def authenticate(token)
    return false if token.nil?
    validate(token)
  end
end
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_ruby::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let decapitated = AstDecapitator::decapitate(code, LanguageKind::Ruby, &mut parser, false);
        assert!(decapitated.contains("# Contract docstring to keep"));
        assert!(decapitated.contains("def authenticate(token)"));
        assert!(decapitated.contains("# stripped"));
        assert!(!decapitated.contains("validate(token)"));
        assert_eq!(
            LanguageKind::from_path("app/models/auth.rb"),
            LanguageKind::Ruby
        );
        assert_eq!(LanguageKind::Ruby.as_str(), "ruby");
    }

    #[test]
    fn test_php_decapitation() {
        let code = r#"<?php
class AuthController {
    #[Route('/login')]
    public function login($req) {
        $token = $this->authService->generate($req);
        return $token;
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_php::LANGUAGE_PHP.into();
        parser.set_language(&lang).unwrap();
        let decapitated = AstDecapitator::decapitate(code, LanguageKind::Php, &mut parser, false);
        assert!(decapitated.contains("#[Route('/login')]"));
        assert!(decapitated.contains("public function login($req) { /* stripped */ }"));
        assert!(!decapitated.contains("authService->generate"));
        assert_eq!(
            LanguageKind::from_path("src/Controller/Auth.php"),
            LanguageKind::Php
        );
        assert_eq!(LanguageKind::Php.as_str(), "php");
    }

    #[test]
    fn test_swift_decapitation() {
        let code = r#"
class AuthService {
    func authenticate(token: String) -> Bool {
        if token.isEmpty {
            return false
        }
        return validate(token)
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_swift::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let decapitated = AstDecapitator::decapitate(code, LanguageKind::Swift, &mut parser, false);
        assert!(decapitated.contains("func authenticate(token: String) -> Bool { /* stripped */ }"));
        assert!(!decapitated.contains("validate(token)"));
        assert_eq!(
            LanguageKind::from_path("Sources/App/Auth.swift"),
            LanguageKind::Swift
        );
        assert_eq!(LanguageKind::Swift.as_str(), "swift");
    }

    #[test]
    fn test_scala_decapitation() {
        let code = r#"
class AuthService {
  def authenticate(token: String): Boolean = {
    val valid = token.nonEmpty
    valid
  }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_scala::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let decapitated = AstDecapitator::decapitate(code, LanguageKind::Scala, &mut parser, false);
        assert!(
            decapitated.contains("def authenticate(token: String): Boolean = { /* stripped */ }")
        );
        assert!(!decapitated.contains("token.nonEmpty"));

        let concise = "object Foo {\n  def bar(x: Int): Int = x + 1\n}\n";
        let mut parser2 = Parser::new();
        parser2.set_language(&lang).unwrap();
        let decapitated_concise =
            AstDecapitator::decapitate(concise, LanguageKind::Scala, &mut parser2, false);
        assert!(decapitated_concise.contains("def bar(x: Int): Int = /* stripped */"));
        assert!(!decapitated_concise.contains("x + 1"));

        assert_eq!(
            LanguageKind::from_path("src/main/scala/Auth.scala"),
            LanguageKind::Scala
        );
        assert_eq!(LanguageKind::Scala.as_str(), "scala");
    }

    #[test]
    fn test_decapitate_auto_preserves_on_include_body() {
        let code = "fn compute() -> i32 { let x = 42; x * 2 }";
        let out = AstDecapitator::decapitate_auto(code, LanguageKind::Rust, true);
        assert_eq!(out, code);
    }

    #[test]
    fn test_typescript_arrow_functions_decapitation() {
        let code = r#"
export const processRefund = async (req: Request): Promise<Response> => {
    const amount = req.body.amount;
    for (let i = 0; i < 100; i++) {
        total += i;
    }
    return { status: "ok" };
};

export const computeDiscount = (rate: number) => rate * 0.15;
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();

        let decapitated =
            AstDecapitator::decapitate(code, LanguageKind::TypeScript, &mut parser, false);

        assert!(
            decapitated.contains("export const processRefund = async (req: Request): Promise<Response> => { /* stripped */ };"),
            "Arrow function with statement block body must be decapitated to stripped block"
        );
        assert!(
            !decapitated.contains("req.body.amount"),
            "Arrow function imperative body must not leak"
        );
        assert!(
            decapitated
                .contains("export const computeDiscount = (rate: number) => /* stripped */;"),
            "Concise arrow function must be decapitated"
        );
        assert!(!decapitated.contains("rate * 0.15"));
    }

    #[test]
    fn test_deeply_nested_cst_does_not_overflow_stack() {
        // Regression test for the rust-lang/rust benchmark crash: a long chain of binary
        // operators produces a CST hundreds/thousands of levels deep (one binary_expression
        // node per operator) while using zero '{', '(', '[' characters, so it sails straight
        // past AstGuard::max_nesting_depth's lexical bracket count (a proxy, not a real CST
        // depth check) and would previously blow the native stack in
        // `collect_body_replacements`, which had no depth limit of its own.
        let deep_chain = (0..5_000).map(|_| "1").collect::<Vec<_>>().join(" + ");
        let code = format!("const DEEP: i32 = {deep_chain};");

        assert!(
            AstGuard::max_nesting_depth(code.as_bytes()) <= AstGuard::MAX_NESTING_DEPTH,
            "test fixture must pass the lexical guard to actually exercise the CST-depth cap"
        );

        let mut parser = Parser::new();
        let lang = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        // Must return without panicking or aborting the process.
        let _ = AstDecapitator::decapitate(&code, LanguageKind::Rust, &mut parser, false);
    }

    #[test]
    fn test_parser_timeout_bounded_stub() {
        // Verify bounded error stub invariant: must be <= 256 bytes protecting LLM context
        assert!(AstDecapitator::BOUNDED_ERROR_STUB.len() <= 256);

        let mut parser = Parser::new();
        let lang = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        // Set an immediate 1 microsecond timeout to guarantee timeout trigger
        parser.set_timeout_micros(1);

        // Huge simulated unparsable or timeout payload
        let huge_code =
            "fn heavy_computation() { ".to_string() + &"let x = 1; ".repeat(10_000) + "}";
        let result = AstDecapitator::decapitate(&huge_code, LanguageKind::Rust, &mut parser, false);

        // On timeout, result MUST be the compact bounded stub, NEVER the huge raw file
        assert_eq!(result, AstDecapitator::BOUNDED_ERROR_STUB);
        assert!(result.len() <= 256);
    }

    #[test]
    fn test_typescript_inferred_return_type_synthesis() {
        let code = r#"
export const fetchBillingRecord = (tenantId: string) => {
    return { id: "rec_123", status: "PAID", balance: 0.00 };
};

export const getUserAccount = (id: string) => ({
    id,
    email: "user@corp.com",
    tier: "ENTERPRISE",
});
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();

        let decapitated =
            AstDecapitator::decapitate(code, LanguageKind::TypeScript, &mut parser, false);

        // Verify statement block return type synthesis
        assert!(
            decapitated.contains("export const fetchBillingRecord = (tenantId: string): { balance: any, id: any, status: any } => { /* stripped */ };"),
            "Should synthesize inferred return type for statement block arrow function: got: {}",
            decapitated
        );

        // Verify concise arrow function return type synthesis
        assert!(
            decapitated.contains("export const getUserAccount = (id: string): { email: any, id: any, tier: any } => /* stripped */;"),
            "Should synthesize inferred return type for concise arrow function: got: {}",
            decapitated
        );

        // Imperative literal values must not leak
        assert!(!decapitated.contains("rec_123"));
        assert!(!decapitated.contains("user@corp.com"));
    }
}
