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
    /// Compact bounded stub (122 bytes <= 256 bytes) protecting LLM context on parser timeout or resource exhaustion
    pub const BOUNDED_ERROR_STUB: &'static str =
        "// [MeshMCP Warning: AST decapitation bounded or parser timeout (>15ms). Body stripped to protect context window.]\n";

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
            LanguageKind::Protobuf | LanguageKind::Yaml => return content.to_string(),
            _ => {
                if content.len() > 1024 {
                    return Self::BOUNDED_ERROR_STUB.to_string();
                } else {
                    return content.to_string();
                }
            }
        };

        if let Ok(mut parser) = parser_res {
            Self::decapitate(content, lang_kind, &mut parser, false)
        } else {
            Self::BOUNDED_ERROR_STUB.to_string()
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
        Self::collect_body_replacements(content, tree.root_node(), lang_kind, &mut replacements);

        if replacements.is_empty() {
            return content.to_string();
        }

        // Sort replacements in reverse order of start byte to apply bottom-up.
        // For identical start bytes (e.g. insertion at start of body), sort by end byte descending.
        replacements.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

        let mut result = content.to_string();
        for (start_byte, end_byte, replacement) in replacements {
            if start_byte < result.len() && end_byte <= result.len() && start_byte <= end_byte {
                result.replace_range(start_byte..end_byte, &replacement);
            }
        }

        result
    }

    fn collect_body_replacements(
        source: &str,
        node: Node,
        lang_kind: LanguageKind,
        replacements: &mut Vec<(usize, usize, std::borrow::Cow<'static, str>)>,
    ) {
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
                                replacements.push((pos, pos, std::borrow::Cow::Owned(synthetic_type)));
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
            LanguageKind::Python if kind == "function_definition" => {
                if let Some(body) = node.child_by_field_name("body") {
                    replacements.push((
                        body.start_byte(),
                        body.end_byte(),
                        std::borrow::Cow::Borrowed(" ..."),
                    ));
                    return;
                }
            }
            _ => {}
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_body_replacements(source, child, lang_kind, replacements);
        }
    }

    /// Recursively collects key names from returned object literals to synthesize inferred return types
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
