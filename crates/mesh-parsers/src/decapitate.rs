use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageKind {
    Java,
    Go,
    Python,
    TypeScript,
    Rust,
    Protobuf,
    Yaml,
    Unknown,
}

impl LanguageKind {
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
        } else if path_str.ends_with(".yaml") || path_str.ends_with(".yml") {
            Self::Yaml
        } else {
            Self::Unknown
        }
    }
}

/// Polyglot AST Decapitation Engine per RFC-001 Section 4.4
pub struct AstDecapitator;

impl AstDecapitator {
    /// Decapitates imperative method bodies using the appropriate language parser automatically
    pub fn decapitate_auto(content: &str, lang_kind: LanguageKind, include_body: bool) -> String {
        if include_body {
            return content.to_string();
        }

        let parser_res = match lang_kind {
            LanguageKind::Java => {
                crate::guard::AstGuard::create_bounded_parser(&tree_sitter_java::LANGUAGE.into())
            }
            LanguageKind::Go => {
                crate::guard::AstGuard::create_bounded_parser(&tree_sitter_go::LANGUAGE.into())
            }
            LanguageKind::Python => {
                crate::guard::AstGuard::create_bounded_parser(&tree_sitter_python::LANGUAGE.into())
            }
            LanguageKind::TypeScript => crate::guard::AstGuard::create_bounded_parser(
                &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            ),
            LanguageKind::Rust => {
                crate::guard::AstGuard::create_bounded_parser(&tree_sitter_rust::LANGUAGE.into())
            }
            _ => return content.to_string(),
        };

        if let Ok(mut parser) = parser_res {
            Self::decapitate(content, lang_kind, &mut parser, false)
        } else {
            content.to_string()
        }
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
            None => return content.to_string(),
        };

        let mut replacements = Vec::new();
        Self::collect_body_replacements(tree.root_node(), lang_kind, &mut replacements);

        if replacements.is_empty() {
            return content.to_string();
        }

        // Sort replacements in reverse order of start byte to apply bottom-up
        replacements.sort_by_key(|a| std::cmp::Reverse(a.0));

        let mut result = content.to_string();
        for (start_byte, end_byte, replacement) in replacements {
            if start_byte < result.len() && end_byte <= result.len() && start_byte <= end_byte {
                result.replace_range(start_byte..end_byte, replacement);
            }
        }

        result
    }

    fn collect_body_replacements(
        node: Node,
        lang_kind: LanguageKind,
        replacements: &mut Vec<(usize, usize, &'static str)>,
    ) {
        let kind = node.kind();

        match lang_kind {
            LanguageKind::Java
                if kind == "method_declaration" || kind == "constructor_declaration" =>
            {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((body.start_byte(), body.end_byte(), "{ /* stripped */ }"));
                    return;
                }
            }
            LanguageKind::Go if kind == "function_declaration" || kind == "method_declaration" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((body.start_byte(), body.end_byte(), "{ /* stripped */ }"));
                    return;
                }
            }
            LanguageKind::TypeScript
                if kind == "method_definition"
                    || kind == "function_declaration"
                    || kind == "function_item" =>
            {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((body.start_byte(), body.end_byte(), "{ /* stripped */ }"));
                    return;
                }
            }
            LanguageKind::Rust if kind == "function_item" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((body.start_byte(), body.end_byte(), "{ /* stripped */ }"));
                    return;
                }
            }
            LanguageKind::Python if kind == "function_definition" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((body.start_byte(), body.end_byte(), " ..."));
                    return;
                }
            }
            _ => {}
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_body_replacements(child, lang_kind, replacements);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
