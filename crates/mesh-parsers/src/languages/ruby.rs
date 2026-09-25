use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Tree};

/// Ruby/Rails extractor. Emits `ServiceClass` for classes, `Interface` for
/// modules (Ruby's mixin construct is the closest analogue), `HttpEndpoint`
/// for Rails controller actions and `config/routes.rb`-style route DSL calls.
pub struct RubyExtractor;

impl RubyExtractor {
    /// Extracts from an already-parsed tree. `PolyglotIndexer` (production
    /// indexing) parses once via `AstGuard::parse_with`, so a parse failure is
    /// visible instead of silently producing an empty result indistinguishable
    /// from a legitimately empty file; tests parse `content` themselves first.
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        tree: &Tree,
    ) -> Vec<ContractNode> {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let package_name = mesh_core::detect_service_package(&file_path, None);
        let path_str = file_path.to_string_lossy().to_lowercase();
        let is_routes_file = path_str.contains("routes");

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &package_name,
            is_routes_file,
            false,
            &mut nodes,
            0,
        );
        nodes
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        is_routes_file: bool,
        in_controller: bool,
        nodes: &mut Vec<ContractNode>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

        let mut child_in_controller = in_controller;

        match node.kind() {
            "class" => {
                let class_name = Self::class_name(node, source).unwrap_or("UnknownClass");
                let is_controller = class_name.ends_with("Controller")
                    || node
                        .child_by_field_name("superclass")
                        .and_then(|n| n.utf8_text(source).ok())
                        .is_some_and(|t| t.contains("Controller"));

                let first_line = Self::first_line(node, source, class_name);
                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(class_name),
                    kind: NodeKind::ServiceClass,
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(first_line)),
                    docstring: None,
                });
                child_in_controller = is_controller;
            }
            "module" => {
                let mod_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownModule");
                let first_line = Self::first_line(node, source, mod_name);
                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(mod_name),
                    kind: NodeKind::Interface,
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(first_line)),
                    docstring: None,
                });
            }
            "method" | "singleton_method" => {
                let method_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("unknown_method");

                let kind = if in_controller
                    && method_name != "initialize"
                    && !method_name.starts_with('_')
                {
                    NodeKind::HttpEndpoint
                } else {
                    NodeKind::ServiceClass
                };

                let first_line = Self::first_line(node, source, method_name);
                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(method_name),
                    kind,
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(first_line)),
                    docstring: None,
                });
                // Method bodies are not scanned for nested route DSL calls.
                return;
            }
            "call" if is_routes_file => {
                if let Some(node_out) = Self::route_call_node(node, source, package_name, repo_id) {
                    nodes.push(ContractNode {
                        id: 0,
                        file_path: file_path.clone(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        ..node_out
                    });
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::visit_node(
                child,
                source,
                file_path,
                repo_id,
                package_name,
                is_routes_file,
                child_in_controller,
                nodes,
                depth + 1,
            );
        }
    }

    /// Recognises `get "/path"`, `post "/path"`, `resources :name`, etc. in a
    /// `config/routes.rb` style DSL call. Returns a partially-filled node
    /// (`file_path`/`line_*` are overwritten by the caller).
    fn route_call_node(
        node: Node,
        source: &[u8],
        package_name: &CompactStr,
        repo_id: RepoId,
    ) -> Option<ContractNode> {
        let method = node
            .child_by_field_name("method")
            .and_then(|n| n.utf8_text(source).ok())?;

        const VERBS: &[&str] = &["get", "post", "put", "patch", "delete"];
        const RESOURCE: &[&str] = &["resources", "resource"];

        if !VERBS.contains(&method) && !RESOURCE.contains(&method) {
            return None;
        }

        let first_arg = node
            .child_by_field_name("arguments")
            .and_then(|args| args.named_child(0))
            .and_then(|arg| arg.utf8_text(source).ok())
            .map(|t| t.trim_matches(|c: char| c == '\'' || c == '"' || c == ':'))
            .unwrap_or("");

        if first_arg.is_empty() {
            return None;
        }

        let name = if VERBS.contains(&method) {
            format!("{} {}", method.to_uppercase(), first_arg)
        } else {
            format!("resources :{first_arg}")
        };

        Some(ContractNode {
            id: 0,
            name: CompactStr::new(&name),
            kind: NodeKind::HttpEndpoint,
            file_path: Arc::from(Path::new("")),
            line_start: 0,
            line_end: 0,
            package: package_name.clone(),
            repo_id,
            signature: Some(CompactStr::new(&name)),
            docstring: None,
        })
    }

    fn class_name<'a>(node: Node<'a>, source: &'a [u8]) -> Option<&'a str> {
        let name_node = node.child_by_field_name("name")?;
        match name_node.kind() {
            "scope_resolution" => name_node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok()),
            _ => name_node.utf8_text(source).ok(),
        }
    }

    fn first_line(node: Node, source: &[u8], fallback: &str) -> String {
        node.utf8_text(source)
            .ok()
            .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
            .unwrap_or_else(|| fallback.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parser() -> Parser {
        let mut parser = Parser::new();
        let lang = tree_sitter_ruby::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_ruby_extractor_controller_actions() {
        let code = r#"
class UsersController < ApplicationController
  def index
    @users = User.all
  end

  def show
    @user = User.find(params[:id])
  end

  private

  def initialize
  end
end
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let nodes = RubyExtractor::extract(
            Path::new("app/controllers/users_controller.rb"),
            code,
            2,
            &tree,
        );

        let find = |name: &str| nodes.iter().find(|n| n.name == name);
        assert_eq!(
            find("UsersController").unwrap().kind,
            NodeKind::ServiceClass
        );
        assert_eq!(find("index").unwrap().kind, NodeKind::HttpEndpoint);
        assert_eq!(find("show").unwrap().kind, NodeKind::HttpEndpoint);
        assert_eq!(find("initialize").unwrap().kind, NodeKind::ServiceClass);
        assert!(nodes.iter().all(|n| n.repo_id == 2));
    }

    #[test]
    fn test_ruby_extractor_module_is_interface() {
        let code = "module Authenticatable\n  def authenticate\n  end\nend\n";
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let nodes =
            RubyExtractor::extract(Path::new("app/models/concerns/auth.rb"), code, 0, &tree);
        assert_eq!(
            nodes
                .iter()
                .find(|n| n.name == "Authenticatable")
                .unwrap()
                .kind,
            NodeKind::Interface
        );
    }

    #[test]
    fn test_ruby_extractor_routes() {
        let code = r#"
Rails.application.routes.draw do
  get '/users', to: 'users#index'
  resources :posts
end
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let nodes = RubyExtractor::extract(Path::new("config/routes.rb"), code, 0, &tree);
        assert!(nodes
            .iter()
            .any(|n| n.name == "GET /users" && n.kind == NodeKind::HttpEndpoint));
        assert!(nodes
            .iter()
            .any(|n| n.name == "resources :posts" && n.kind == NodeKind::HttpEndpoint));
    }

    #[test]
    fn test_ruby_extractor_plain_class_not_controller() {
        let code = "class Widget\n  def price\n    10\n  end\nend\n";
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let nodes = RubyExtractor::extract(Path::new("app/models/widget.rb"), code, 0, &tree);
        assert_eq!(
            nodes.iter().find(|n| n.name == "price").unwrap().kind,
            NodeKind::ServiceClass
        );
    }
}
