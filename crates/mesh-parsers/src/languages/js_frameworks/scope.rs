//! What a JavaScript / TypeScript file says about itself, before any framework
//! detector runs: which modules it loads and under which local names
//! ([`ModuleBindings`]), which string constants it declares ([`ConstTable`]), and
//! how an argument expression is read as a name ([`Names`]).
//!
//! Detectors are gated on the first (`kafkajs` imported? `ioredis` required?) so a
//! generic method name (`publish`, `subscribe`, `send`) never means a broker call
//! in a file that does not talk to that broker.

use super::super::typescript::TypeScriptExtractor;
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

/// Expression depth beyond which a name is not resolved (`'a' + 'b' + …` chains).
const MAX_RESOLVE_DEPTH: usize = 24;

/// `local name -> where it comes from`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub module: String,
    /// The exported name: `default`, `*` (a namespace / whole-module `require`), or a name.
    pub imported: String,
}

/// Every module a file loads (static `import`, re-export, `require`, `import()`)
/// and the local names bound to them.
#[derive(Debug, Default)]
pub(crate) struct ModuleBindings {
    modules: HashSet<String>,
    locals: HashMap<String, Binding>,
}

/// Whether `module` is `lib`, one of its sub-paths (`ioredis/built/Redis`), or,
/// when `lib` ends with `/`, anything under that scope (`@nats-io/`).
fn module_matches(module: &str, lib: &str) -> bool {
    if lib.ends_with('/') {
        return module.starts_with(lib);
    }
    module == lib
        || module
            .strip_prefix(lib)
            .is_some_and(|rest| rest.starts_with('/'))
}

impl ModuleBindings {
    pub(crate) fn collect(root: Node, source: &[u8]) -> Self {
        let mut out = Self::default();
        let mut cursor = root.walk();
        loop {
            out.visit(cursor.node(), source);
            if cursor.goto_first_child() || cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    return out;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn visit(&mut self, node: Node, source: &[u8]) {
        match node.kind() {
            "import_statement" => self.visit_import(node, source),
            "export_statement" => {
                if let Some(module) = node
                    .child_by_field_name("source")
                    .and_then(|n| unquote(n, source))
                {
                    self.modules.insert(module);
                }
            }
            "call_expression" => {
                if let Some(module) = TypeScriptExtractor::module_call_specifier(node, source) {
                    self.modules.insert(module);
                }
            }
            "variable_declarator" => self.visit_declarator(node, source),
            _ => {}
        }
    }

    fn visit_import(&mut self, node: Node, source: &[u8]) {
        let require_clause = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "import_require_clause");
        let module = node
            .child_by_field_name("source")
            .or_else(|| require_clause.and_then(|c| c.child_by_field_name("source")))
            .and_then(|n| unquote(n, source));
        let Some(module) = module else { return };
        self.modules.insert(module.clone());
        let bind = |locals: &mut HashMap<String, Binding>, local: Node, imported: &str| {
            if let Ok(name) = local.utf8_text(source) {
                locals.insert(
                    name.to_string(),
                    Binding {
                        module: module.clone(),
                        imported: imported.to_string(),
                    },
                );
            }
        };
        if let Some(clause) = require_clause {
            if let Some(local) = clause.named_child(0).filter(|n| n.kind() == "identifier") {
                bind(&mut self.locals, local, "*");
            }
        }
        for clause in node
            .children(&mut node.walk())
            .filter(|c| c.kind() == "import_clause")
        {
            for part in clause.children(&mut clause.walk()) {
                match part.kind() {
                    "identifier" => bind(&mut self.locals, part, "default"),
                    "namespace_import" => {
                        if let Some(local) = part
                            .named_children(&mut part.walk())
                            .find(|n| n.kind() == "identifier")
                        {
                            bind(&mut self.locals, local, "*");
                        }
                    }
                    "named_imports" => {
                        for spec in part
                            .children(&mut part.walk())
                            .filter(|s| s.kind() == "import_specifier")
                        {
                            let Some(imported) = spec
                                .child_by_field_name("name")
                                .and_then(|n| n.utf8_text(source).ok())
                            else {
                                continue;
                            };
                            let local = spec
                                .child_by_field_name("alias")
                                .or(spec.child_by_field_name("name"));
                            if let Some(local) = local {
                                bind(&mut self.locals, local, imported);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// `const x = require('m')`, `const { a, b: c } = require('m')`,
    /// `const y = require('m').foo`, `const z = await import('m')`.
    fn visit_declarator(&mut self, node: Node, source: &[u8]) {
        let (Some(name), Some(value)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("value"),
        ) else {
            return;
        };
        let Some(module) = TypeScriptExtractor::module_load_specifier(value, source) else {
            return;
        };
        // `require('m').foo` binds `foo` of the module, not the module itself.
        let mut inner = TypeScriptExtractor::peel_wrappers(value);
        let mut selected: Option<String> = None;
        while inner.kind() == "member_expression" {
            if let Some(p) = inner
                .child_by_field_name("property")
                .and_then(|p| p.utf8_text(source).ok())
            {
                selected = Some(p.to_string());
            }
            match inner.child_by_field_name("object") {
                Some(o) => inner = TypeScriptExtractor::peel_wrappers(o),
                None => break,
            }
        }
        match name.kind() {
            "identifier" => {
                if let Ok(local) = name.utf8_text(source) {
                    self.locals.insert(
                        local.to_string(),
                        Binding {
                            module,
                            imported: selected.unwrap_or_else(|| "*".to_string()),
                        },
                    );
                }
            }
            "object_pattern" => {
                for member in name.named_children(&mut name.walk()) {
                    let (key, local) = match member.kind() {
                        "shorthand_property_identifier_pattern" => (Some(member), Some(member)),
                        "object_assignment_pattern" => {
                            let left = member.child_by_field_name("left");
                            (left, left)
                        }
                        "pair_pattern" => (
                            member.child_by_field_name("key"),
                            member.child_by_field_name("value"),
                        ),
                        _ => (None, None),
                    };
                    let (Some(key), Some(local)) = (key, local) else {
                        continue;
                    };
                    if let (Ok(key), Ok(local)) = (key.utf8_text(source), local.utf8_text(source)) {
                        self.locals.insert(
                            local.to_string(),
                            Binding {
                                module: module.clone(),
                                imported: key.to_string(),
                            },
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether the file loads any module matching one of `libs`.
    pub(crate) fn imports_any(&self, libs: &[&str]) -> bool {
        self.modules
            .iter()
            .any(|m| libs.iter().any(|lib| module_matches(m, lib)))
    }

    pub(crate) fn binding(&self, local: &str) -> Option<&Binding> {
        self.locals.get(local)
    }

    /// Whether `module` is `lib` or under it (see [`module_matches`]).
    pub(crate) fn module_in(module: &str, libs: &[&str]) -> bool {
        libs.iter().any(|lib| module_matches(module, lib))
    }
}

/// A string literal's value (no escapes: a literal holding a backslash is not a
/// name this analysis trusts).
pub(crate) fn unquote(node: Node, source: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let text = node.utf8_text(source).ok()?;
    let inner = text
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .or_else(|| text.strip_prefix('"').and_then(|t| t.strip_suffix('"')))?;
    (!inner.contains('\\')).then(|| inner.to_string())
}

/// Dotted text of an identifier / member chain (`Topics.ORDERS`); `None` for
/// anything else (calls, `this`, computed access).
fn dotted(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(source).ok().map(str::to_string),
        "member_expression" => {
            let object = dotted(node.child_by_field_name("object")?, source)?;
            let property = node.child_by_field_name("property")?;
            (property.kind() == "property_identifier")
                .then(|| property.utf8_text(source).ok())
                .flatten()
                .map(|p| format!("{object}.{p}"))
        }
        _ => None,
    }
}

fn is_constant_case(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// String constants a file declares at module level: `const T = 'orders'`,
/// `const Topics = { ORDERS: 'orders' } as const`, `enum Topic { A = 'a' }`,
/// `class T { static readonly A = 'a' }`, keyed by their dotted path (`Topics.ORDERS`).
#[derive(Debug, Default)]
pub(crate) struct ConstTable {
    values: HashMap<String, String>,
}

impl ConstTable {
    pub(crate) fn collect(root: Node, source: &[u8], bindings: &ModuleBindings) -> Self {
        let mut table = Self::default();
        for stmt in root.named_children(&mut root.walk()) {
            let decl = if stmt.kind() == "export_statement" {
                stmt.child_by_field_name("declaration").unwrap_or(stmt)
            } else {
                stmt
            };
            match decl.kind() {
                "lexical_declaration" | "variable_declaration" => {
                    for d in decl
                        .named_children(&mut decl.walk())
                        .filter(|d| d.kind() == "variable_declarator")
                    {
                        let (Some(name), Some(value)) = (
                            d.child_by_field_name("name")
                                .filter(|n| n.kind() == "identifier"),
                            d.child_by_field_name("value"),
                        ) else {
                            continue;
                        };
                        let Ok(name) = name.utf8_text(source) else {
                            continue;
                        };
                        table.add_value(name, value, source, bindings);
                    }
                }
                "enum_declaration" => {
                    let (Some(name), Some(body)) = (
                        decl.child_by_field_name("name"),
                        decl.child_by_field_name("body"),
                    ) else {
                        continue;
                    };
                    let Ok(name) = name.utf8_text(source) else {
                        continue;
                    };
                    for member in body.named_children(&mut body.walk()) {
                        let (Some(key), Some(value)) = (
                            member.child_by_field_name("name"),
                            member.child_by_field_name("value"),
                        ) else {
                            continue;
                        };
                        if let (Some(key), Some(value)) = (
                            member_key(key, source),
                            table.resolve_literal(value, source, bindings, 0),
                        ) {
                            table.values.insert(format!("{name}.{key}"), value);
                        }
                    }
                }
                "class_declaration" => {
                    let (Some(name), Some(body)) = (
                        decl.child_by_field_name("name"),
                        decl.child_by_field_name("body"),
                    ) else {
                        continue;
                    };
                    let Ok(name) = name.utf8_text(source) else {
                        continue;
                    };
                    for field in body
                        .named_children(&mut body.walk())
                        .filter(|f| f.kind() == "public_field_definition")
                    {
                        let is_static = field
                            .children(&mut field.walk())
                            .any(|c| c.kind() == "static" || c.utf8_text(source) == Ok("static"));
                        if !is_static {
                            continue;
                        }
                        let (Some(key), Some(value)) = (
                            field.child_by_field_name("name"),
                            field.child_by_field_name("value"),
                        ) else {
                            continue;
                        };
                        if let (Some(key), Some(value)) = (
                            member_key(key, source),
                            table.resolve_literal(value, source, bindings, 0),
                        ) {
                            table.values.insert(format!("{name}.{key}"), value);
                        }
                    }
                }
                _ => {}
            }
        }
        table
    }

    /// Records `name = value`: a string, or an object literal of strings (nested).
    fn add_value(&mut self, name: &str, value: Node, source: &[u8], bindings: &ModuleBindings) {
        let value = TypeScriptExtractor::peel_wrappers(value);
        if value.kind() == "object" {
            self.add_object(name, value, source, bindings, 0);
        } else if let Some(v) = self.resolve_literal(value, source, bindings, 0) {
            self.values.insert(name.to_string(), v);
        }
    }

    fn add_object(
        &mut self,
        prefix: &str,
        object: Node,
        source: &[u8],
        bindings: &ModuleBindings,
        depth: usize,
    ) {
        if depth > 4 {
            return;
        }
        for pair in object
            .named_children(&mut object.walk())
            .filter(|p| p.kind() == "pair")
        {
            let (Some(key), Some(value)) = (
                pair.child_by_field_name("key")
                    .and_then(|k| member_key(k, source)),
                pair.child_by_field_name("value"),
            ) else {
                continue;
            };
            let path = format!("{prefix}.{key}");
            let value = TypeScriptExtractor::peel_wrappers(value);
            if value.kind() == "object" {
                self.add_object(&path, value, source, bindings, depth + 1);
            } else if let Some(v) = self.resolve_literal(value, source, bindings, 0) {
                self.values.insert(path, v);
            }
        }
    }

    /// A value built only from literals and constants already in the table.
    fn resolve_literal(
        &self,
        node: Node,
        source: &[u8],
        bindings: &ModuleBindings,
        depth: usize,
    ) -> Option<String> {
        Names {
            source,
            consts: self,
            bindings,
        }
        .resolve_inner(node, depth, false)
    }
}

/// An object / enum / class member name: `A`, `'a-b'`.
fn member_key(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "property_identifier" | "identifier" => node.utf8_text(source).ok().map(str::to_string),
        "string" => unquote(node, source),
        _ => None,
    }
}

/// Reads argument expressions as names (topics, queues, subjects).
pub(crate) struct Names<'a> {
    pub source: &'a [u8],
    pub consts: &'a ConstTable,
    pub bindings: &'a ModuleBindings,
}

impl Names<'_> {
    /// The name `node` stands for, if it is knowable from this file alone:
    /// a literal, a constant it declares, a concatenation of those, or a reference
    /// to an *imported* constant (`Topics.ORDER_PLACED`, `ORDER_PLACED`), which is kept
    /// as written for the graph to reduce (`normalize_topic_key`) so a producer and
    /// a consumer in two services naming the same member meet. Anything else
    /// (a parameter, `config.topic`, a call) is not a name.
    pub(crate) fn resolve(&self, node: Node) -> Option<String> {
        self.resolve_inner(node, 0, true)
    }

    fn resolve_inner(&self, node: Node, depth: usize, allow_imported: bool) -> Option<String> {
        if depth > MAX_RESOLVE_DEPTH {
            return None;
        }
        let node = TypeScriptExtractor::peel_wrappers(node);
        match node.kind() {
            "string" => unquote(node, self.source),
            "template_string" => {
                let mut out = String::new();
                for part in node.named_children(&mut node.walk()) {
                    match part.kind() {
                        "string_fragment" => out.push_str(part.utf8_text(self.source).ok()?),
                        "template_substitution" => {
                            let inner = part.named_child(0)?;
                            out.push_str(&self.resolve_inner(inner, depth + 1, false)?);
                        }
                        _ => return None,
                    }
                }
                (!out.contains('\\')).then_some(out)
            }
            "binary_expression" => {
                let op = node
                    .child_by_field_name("operator")?
                    .utf8_text(self.source)
                    .ok()?;
                if op != "+" {
                    return None;
                }
                let left =
                    self.resolve_inner(node.child_by_field_name("left")?, depth + 1, false)?;
                let right =
                    self.resolve_inner(node.child_by_field_name("right")?, depth + 1, false)?;
                Some(left + &right)
            }
            "identifier" | "member_expression" => {
                let path = dotted(node, self.source)?;
                if let Some(v) = self.consts.values.get(&path) {
                    return Some(v.clone());
                }
                if !allow_imported {
                    return None;
                }
                let first = path.split('.').next()?;
                let last = path.rsplit('.').next()?;
                let imported = self.bindings.binding(first).is_some();
                let is_member_ref = path.contains('.')
                    && first.starts_with(|c: char| c.is_ascii_uppercase())
                    && is_constant_case(last);
                let is_bare_constant = !path.contains('.') && is_constant_case(&path);
                (imported && (is_member_ref || is_bare_constant)).then_some(path)
            }
            _ => None,
        }
    }

    /// Every name in `node`: one for a scalar, one per element for an array.
    pub(crate) fn resolve_all(&self, node: Node) -> Vec<String> {
        let node = TypeScriptExtractor::peel_wrappers(node);
        if node.kind() == "array" {
            return node
                .named_children(&mut node.walk())
                .filter_map(|e| self.resolve(e))
                .collect();
        }
        self.resolve(node).into_iter().collect()
    }

    /// The value node of `key` in an object literal.
    pub(crate) fn object_value<'t>(&self, object: Node<'t>, key: &str) -> Option<Node<'t>> {
        let object = TypeScriptExtractor::peel_wrappers(object);
        if object.kind() != "object" {
            return None;
        }
        object
            .named_children(&mut object.walk())
            .filter(|p| p.kind() == "pair")
            .find(|p| {
                p.child_by_field_name("key")
                    .and_then(|k| member_key(k, self.source))
                    .as_deref()
                    == Some(key)
            })
            .and_then(|p| p.child_by_field_name("value"))
    }
}
