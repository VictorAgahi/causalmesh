//! What a JavaScript / TypeScript file says about itself, before any framework
//! detector runs: which modules it loads and under which local names
//! ([`ModuleBindings`]), which string constants it declares ([`ConstTable`]), which
//! variables and properties hold a broker client ([`Receivers`]), and how an
//! argument expression is read as a name ([`Names`]).
//!
//! Detectors are gated on the first two (`kafkajs` imported? `ioredis` required?)
//! and, where the file builds a client of that library, on the receiver of the call
//! being that client: a generic `.publish()` / `.send()` on an application event bus
//! never means a broker call in a file that merely also uses Redis for caching.

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
    /// Bound by `import type …` / `import { type X }`: names a type, not a value.
    pub type_only: bool,
}

/// Every module a file loads (static `import`, re-export, `require`, `import()`)
/// and the local names bound to them. A type-only import loads nothing at run time
/// and does not count as the file talking to that library.
#[derive(Debug, Default)]
pub(crate) struct ModuleBindings {
    modules: HashSet<String>,
    locals: HashMap<String, Binding>,
    /// Names the file declares itself at module level (`class Queue`, `function publish`).
    declared: HashSet<String>,
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

fn has_type_keyword(node: Node) -> bool {
    node.children(&mut node.walk()).any(|c| c.kind() == "type")
}

impl ModuleBindings {
    pub(crate) fn collect(root: Node, source: &[u8]) -> Self {
        let mut out = Self::default();
        for stmt in root.named_children(&mut root.walk()) {
            let decl = if stmt.kind() == "export_statement" {
                stmt.child_by_field_name("declaration").unwrap_or(stmt)
            } else {
                stmt
            };
            if matches!(
                decl.kind(),
                "class_declaration"
                    | "abstract_class_declaration"
                    | "function_declaration"
                    | "generator_function_declaration"
            ) {
                if let Some(name) = decl
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    out.declared.insert(name.to_string());
                }
            }
        }
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
                    if !has_type_keyword(node) {
                        self.modules.insert(module);
                    }
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
        let statement_is_type = has_type_keyword(node);
        if !statement_is_type {
            self.modules.insert(module.clone());
        }
        let bind = |locals: &mut HashMap<String, Binding>,
                    local: Node,
                    imported: &str,
                    type_only: bool| {
            if let Ok(name) = local.utf8_text(source) {
                locals.insert(
                    name.to_string(),
                    Binding {
                        module: module.clone(),
                        imported: imported.to_string(),
                        type_only,
                    },
                );
            }
        };
        if let Some(clause) = require_clause {
            if let Some(local) = clause.named_child(0).filter(|n| n.kind() == "identifier") {
                bind(&mut self.locals, local, "*", statement_is_type);
            }
        }
        for clause in node
            .children(&mut node.walk())
            .filter(|c| c.kind() == "import_clause")
        {
            for part in clause.children(&mut clause.walk()) {
                match part.kind() {
                    "identifier" => bind(&mut self.locals, part, "default", statement_is_type),
                    "namespace_import" => {
                        if let Some(local) = part
                            .named_children(&mut part.walk())
                            .find(|n| n.kind() == "identifier")
                        {
                            bind(&mut self.locals, local, "*", statement_is_type);
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
                                bind(
                                    &mut self.locals,
                                    local,
                                    imported,
                                    statement_is_type || has_type_keyword(spec),
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // A named specifier of a statement that is not type-only still loads the module.
        if !statement_is_type {
            self.modules.insert(module);
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
                            type_only: false,
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
                                type_only: false,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether the file loads (at run time) any module matching one of `libs`.
    pub(crate) fn imports_any(&self, libs: &[&str]) -> bool {
        self.modules
            .iter()
            .any(|m| libs.iter().any(|lib| module_matches(m, lib)))
    }

    /// The binding of `local`, whether a value or a type.
    pub(crate) fn binding(&self, local: &str) -> Option<&Binding> {
        self.locals.get(local)
    }

    /// The binding of `local` when it names a value (not `import type`).
    pub(crate) fn value_binding(&self, local: &str) -> Option<&Binding> {
        self.locals.get(local).filter(|b| !b.type_only)
    }

    /// Whether the file declares `name` itself at module level.
    pub(crate) fn declares(&self, name: &str) -> bool {
        self.declared.contains(name)
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

/// Dotted text of an identifier / member chain (`Topics.ORDERS`, `Topics['q-z']`,
/// `this.topic`); `None` for anything else (calls, computed access).
fn dotted(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "shorthand_property_identifier" | "this" => {
            node.utf8_text(source).ok().map(str::to_string)
        }
        "member_expression" => {
            let object = dotted(node.child_by_field_name("object")?, source)?;
            let property = node.child_by_field_name("property")?;
            (property.kind() == "property_identifier")
                .then(|| property.utf8_text(source).ok())
                .flatten()
                .map(|p| format!("{object}.{p}"))
        }
        "subscript_expression" => {
            let object = dotted(node.child_by_field_name("object")?, source)?;
            let key = unquote(node.child_by_field_name("index")?, source)?;
            Some(format!("{object}.{key}"))
        }
        _ => None,
    }
}

fn is_constant_case(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// An object / enum / class member name: `A`, `'a-b'`.
fn member_key(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "property_identifier" | "private_property_identifier" | "identifier" => {
            node.utf8_text(source).ok().map(str::to_string)
        }
        "string" => unquote(node, source),
        _ => None,
    }
}

/// `Object.freeze({ … })` is the object it freezes.
fn unfreeze<'t>(node: Node<'t>, source: &[u8]) -> Node<'t> {
    let node = TypeScriptExtractor::peel_wrappers(node);
    if node.kind() == "call_expression"
        && node
            .child_by_field_name("function")
            .and_then(|f| dotted(f, source))
            .as_deref()
            == Some("Object.freeze")
    {
        if let Some(arg) = node
            .child_by_field_name("arguments")
            .and_then(|a| a.named_child(0))
        {
            return TypeScriptExtractor::peel_wrappers(arg);
        }
    }
    node
}

fn is_const_declaration(decl: Node) -> bool {
    decl.kind() == "lexical_declaration" && decl.child(0).is_some_and(|c| c.kind() == "const")
}

/// String constants a file declares at module level with `const`:
/// `const T = 'orders'`, `const Topics = { ORDERS: 'orders' } as const`,
/// `enum Topic { A = 'a' }`, `class T { static readonly A = 'a' }`, keyed by their
/// dotted path (`Topics.ORDERS`). `let` and `var` are never recorded: they can change.
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
                "lexical_declaration" if is_const_declaration(decl) => {
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
                            table.resolve_literal(value, source, bindings),
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
                            .any(|c| c.utf8_text(source) == Ok("static"));
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
                            table.resolve_literal(value, source, bindings),
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
        let value = unfreeze(value, source);
        if value.kind() == "object" {
            self.add_object(name, value, source, bindings, 0);
        } else if let Some(v) = self.resolve_literal(value, source, bindings) {
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
            let value = unfreeze(value, source);
            if value.kind() == "object" {
                self.add_object(&path, value, source, bindings, depth + 1);
            } else if let Some(v) = self.resolve_literal(value, source, bindings) {
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
    ) -> Option<String> {
        Names {
            source,
            consts: self,
            bindings,
        }
        .resolve_inner(node, 0, false)
    }
}

// ---- local scopes -------------------------------------------------------------------

/// How a name is declared in a scope enclosing a use site.
enum Local<'t> {
    /// A parameter, a non-constant local, a function or a class: not a known constant.
    Shadowed,
    /// `const name = <value>` in an enclosing block.
    Const(Node<'t>),
}

fn pattern_binds(pattern: Node, name: &str, source: &[u8]) -> bool {
    TypeScriptExtractor::binds_name(pattern, name, source, 0)
}

/// The innermost declaration of `name` visible at `at` inside a function or block
/// (module level is [`ConstTable`]'s). A function parameter or a block-level
/// declaration of the same name hides the module-level constant.
fn local_declaration<'t>(at: Node<'t>, name: &str, source: &[u8]) -> Option<Local<'t>> {
    let mut ancestor = at.parent();
    while let Some(a) = ancestor {
        match a.kind() {
            "function_declaration"
            | "function_expression"
            | "generator_function"
            | "generator_function_declaration"
            | "arrow_function"
            | "method_definition" => {
                let params = a
                    .child_by_field_name("parameters")
                    .or_else(|| a.child_by_field_name("parameter"));
                if params.is_some_and(|p| {
                    p.utf8_text(source).ok() == Some(name) || pattern_binds(p, name, source)
                }) {
                    return Some(Local::Shadowed);
                }
            }
            "catch_clause" => {
                if a.child_by_field_name("parameter")
                    .is_some_and(|p| pattern_binds(p, name, source))
                {
                    return Some(Local::Shadowed);
                }
            }
            "for_in_statement" => {
                if a.child_by_field_name("left")
                    .is_some_and(|l| pattern_binds(l, name, source))
                {
                    return Some(Local::Shadowed);
                }
            }
            "statement_block" => {
                for stmt in a.named_children(&mut a.walk()) {
                    match stmt.kind() {
                        "lexical_declaration" | "variable_declaration" => {
                            for d in stmt
                                .named_children(&mut stmt.walk())
                                .filter(|d| d.kind() == "variable_declarator")
                            {
                                let Some(n) = d.child_by_field_name("name") else {
                                    continue;
                                };
                                if !pattern_binds(n, name, source) {
                                    continue;
                                }
                                return Some(match d.child_by_field_name("value") {
                                    Some(v)
                                        if is_const_declaration(stmt)
                                            && n.kind() == "identifier" =>
                                    {
                                        Local::Const(v)
                                    }
                                    _ => Local::Shadowed,
                                });
                            }
                        }
                        "function_declaration" | "class_declaration"
                            if stmt
                                .child_by_field_name("name")
                                .and_then(|n| n.utf8_text(source).ok())
                                == Some(name) =>
                        {
                            return Some(Local::Shadowed);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        ancestor = a.parent();
    }
    None
}

/// Whether `name` at `at` is hidden by a parameter or local declaration of the
/// same name (so an import or module-level constant of that name is not what it denotes).
pub(crate) fn is_shadowed(at: Node, name: &str, source: &[u8]) -> bool {
    local_declaration(at, name, source).is_some()
}

/// The initializer of `this.<field>` in the class enclosing `at`.
fn this_field<'t>(at: Node<'t>, field: &str, source: &[u8]) -> Option<Node<'t>> {
    let mut ancestor = at.parent();
    while let Some(a) = ancestor {
        if matches!(
            a.kind(),
            "class_declaration" | "class" | "abstract_class_declaration"
        ) {
            let body = a.child_by_field_name("body")?;
            return body
                .named_children(&mut body.walk())
                .filter(|f| matches!(f.kind(), "public_field_definition" | "field_definition"))
                .find(|f| {
                    f.child_by_field_name("name")
                        .or_else(|| f.child_by_field_name("property"))
                        .and_then(|n| member_key(n, source))
                        .as_deref()
                        == Some(field)
                })
                .and_then(|f| f.child_by_field_name("value"));
        }
        ancestor = a.parent();
    }
    None
}

// ---- broker clients -------------------------------------------------------------------

/// The identifier a receiver / initializer chain starts from: `redis` in
/// `redis.duplicate()`, `redis` (the property) in `this.redis.pipeline()`.
pub(crate) fn root_key(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "await_expression"
        | "parenthesized_expression"
        | "as_expression"
        | "satisfies_expression"
        | "non_null_expression" => root_key(node.named_child(0)?, source),
        "call_expression" => root_key(node.child_by_field_name("function")?, source),
        "new_expression" => root_key(node.child_by_field_name("constructor")?, source),
        "member_expression" => {
            let object = node.child_by_field_name("object")?;
            if object.kind() == "this" {
                node.child_by_field_name("property")?
                    .utf8_text(source)
                    .ok()
                    .map(str::to_string)
            } else {
                root_key(object, source)
            }
        }
        "identifier" => node.utf8_text(source).ok().map(str::to_string),
        _ => None,
    }
}

/// The name a call's receiver is known by: `pub` in `pub.publish()`, `redis` in
/// `this.redis.publish()` and `this.redis.pipeline().publish()`.
pub(crate) fn receiver_key(receiver: Node, source: &[u8]) -> Option<String> {
    match receiver.kind() {
        "member_expression" => {
            let object = receiver.child_by_field_name("object")?;
            if object.kind() == "this" {
                return receiver
                    .child_by_field_name("property")?
                    .utf8_text(source)
                    .ok()
                    .map(str::to_string);
            }
            // `this.clients.redis` is known as `redis`.
            receiver
                .child_by_field_name("property")?
                .utf8_text(source)
                .ok()
                .map(str::to_string)
        }
        _ => root_key(receiver, source),
    }
}

/// A variable, property or parameter and what it is initialised from / typed as.
struct Declaration<'t> {
    key: String,
    value: Option<Node<'t>>,
    types: Vec<String>,
    /// Decorated `@Inject(…)` / `@InjectQueue(…)`-style: the container supplies it.
    injected: bool,
}

/// Which names hold a client of which library family, and which names are queues.
#[derive(Debug, Default)]
pub(crate) struct Receivers {
    /// Per family (its first library name), the names bound to a client of it.
    by_family: HashMap<&'static str, HashSet<String>>,
    /// `const q = new Bull('emails')` -> `q => emails`.
    queue_names: HashMap<String, String>,
}

fn type_identifiers(node: Node, source: &[u8], out: &mut Vec<String>) {
    let mut cursor = node.walk();
    loop {
        let n = cursor.node();
        if matches!(n.kind(), "type_identifier" | "identifier") {
            if let Ok(t) = n.utf8_text(source) {
                out.push(t.to_string());
            }
        }
        if cursor.goto_first_child() || cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn has_inject_decorator(node: Node, source: &[u8]) -> bool {
    let decorated = |n: Node| {
        n.kind() == "decorator"
            && n.utf8_text(source)
                .is_ok_and(|t| t.trim_start_matches('@').starts_with("Inject"))
    };
    node.children(&mut node.walk()).any(decorated) || node.prev_sibling().is_some_and(decorated)
}

impl Receivers {
    pub(crate) fn collect(
        root: Node,
        source: &[u8],
        bindings: &ModuleBindings,
        consts: &ConstTable,
        families: &[&'static [&'static str]],
    ) -> Self {
        let mut decls: Vec<Declaration> = Vec::new();
        let mut cursor = root.walk();
        'walk: loop {
            let n = cursor.node();
            let key_and_value = match n.kind() {
                "variable_declarator" => n
                    .child_by_field_name("name")
                    .filter(|k| k.kind() == "identifier")
                    .map(|k| (k, n.child_by_field_name("value"))),
                "public_field_definition" | "field_definition" => n
                    .child_by_field_name("name")
                    .or_else(|| n.child_by_field_name("property"))
                    .map(|k| (k, n.child_by_field_name("value"))),
                "required_parameter" | "optional_parameter" => n
                    .child_by_field_name("pattern")
                    .filter(|k| k.kind() == "identifier")
                    .map(|k| (k, n.child_by_field_name("value"))),
                "assignment_expression" => n
                    .child_by_field_name("left")
                    .filter(|l| l.kind() == "member_expression" || l.kind() == "identifier")
                    .map(|l| (l, n.child_by_field_name("right"))),
                _ => None,
            };
            if let Some((key_node, value)) = key_and_value {
                let key = if key_node.kind() == "member_expression" {
                    key_node
                        .child_by_field_name("property")
                        .and_then(|p| p.utf8_text(source).ok())
                } else {
                    key_node.utf8_text(source).ok()
                };
                if let Some(key) = key {
                    let mut types = Vec::new();
                    if let Some(t) = n.child_by_field_name("type") {
                        type_identifiers(t, source, &mut types);
                    }
                    decls.push(Declaration {
                        key: key.to_string(),
                        value,
                        types,
                        injected: has_inject_decorator(n, source),
                    });
                }
            }
            if cursor.goto_first_child() || cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    break 'walk;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        let mut by_family: HashMap<&'static str, HashSet<String>> = HashMap::new();
        for libs in families {
            let Some(family) = libs.first().copied() else {
                continue;
            };
            if !bindings.imports_any(libs)
                && !decls.iter().any(|d| {
                    d.types.iter().any(|t| {
                        bindings
                            .binding(t)
                            .is_some_and(|b| ModuleBindings::module_in(&b.module, libs))
                    })
                })
            {
                continue;
            }
            let mut bound: HashSet<String> = HashSet::new();
            // Two passes reach `const conn = await amqp.connect(); const ch = conn.createChannel()`.
            for _ in 0..3 {
                for d in &decls {
                    if bound.contains(&d.key) {
                        continue;
                    }
                    let typed = d.types.iter().any(|t| {
                        bindings
                            .binding(t)
                            .is_some_and(|b| ModuleBindings::module_in(&b.module, libs))
                    });
                    let built = d
                        .value
                        .and_then(|v| root_key(v, source))
                        .is_some_and(|root| {
                            bound.contains(&root)
                                || bindings
                                    .value_binding(&root)
                                    .is_some_and(|b| ModuleBindings::module_in(&b.module, libs))
                        });
                    let injected =
                        d.injected && ModuleBindings::module_in("@nestjs/microservices", libs);
                    if typed || built || injected {
                        bound.insert(d.key.clone());
                    }
                }
            }
            if !bound.is_empty() {
                by_family.insert(family, bound);
            }
        }

        // `const q = new Bull('emails')`: the queue a variable stands for.
        let mut queue_names = HashMap::new();
        let names = Names {
            source,
            consts,
            bindings,
        };
        for d in &decls {
            let Some(value) = d.value.map(|v| TypeScriptExtractor::peel_wrappers(v)) else {
                continue;
            };
            if value.kind() != "new_expression" {
                continue;
            }
            let from_queue_lib = value
                .child_by_field_name("constructor")
                .and_then(|c| root_key(c, source))
                .and_then(|r| bindings.value_binding(&r))
                .is_some_and(|b| {
                    ModuleBindings::module_in(&b.module, &["bull", "bee-queue", "bullmq"])
                });
            if !from_queue_lib {
                continue;
            }
            if let Some(name) = value
                .child_by_field_name("arguments")
                .and_then(|a| a.named_child(0))
                .and_then(|a| names.resolve(a))
            {
                queue_names.insert(d.key.clone(), name);
            }
        }
        Self {
            by_family,
            queue_names,
        }
    }

    /// Whether `key` holds a client of the family led by `family`. `None` when the
    /// file builds no such client at all (a client received as an untyped
    /// parameter): the caller then falls back to the file importing the library.
    pub(crate) fn is_client(&self, family: &str, key: &str) -> Option<bool> {
        self.by_family.get(family).map(|set| set.contains(key))
    }

    pub(crate) fn queue_name(&self, key: &str) -> Option<&str> {
        self.queue_names.get(key).map(String::as_str)
    }
}

// ---- names ----------------------------------------------------------------------------

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
        let node = unfreeze(node, self.source);
        match node.kind() {
            "string" => unquote(node, self.source).filter(|s| !s.is_empty()),
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
                (!out.contains('\\') && !out.is_empty()).then_some(out)
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
            "identifier"
            | "shorthand_property_identifier"
            | "member_expression"
            | "subscript_expression" => {
                let path = dotted(node, self.source)?;
                let first = path.split('.').next()?;
                if first == "this" {
                    let field = path.strip_prefix("this.").filter(|f| !f.contains('.'))?;
                    let value = this_field(node, field, self.source)?;
                    return self.resolve_inner(value, depth + 1, false);
                }
                match local_declaration(node, first, self.source) {
                    Some(Local::Shadowed) => return None,
                    Some(Local::Const(value)) if !path.contains('.') => {
                        return self.resolve_inner(value, depth + 1, false);
                    }
                    Some(Local::Const(_)) => return None,
                    None => {}
                }
                if let Some(v) = self.consts.values.get(&path) {
                    return Some(v.clone());
                }
                if !allow_imported {
                    return None;
                }
                let last = path.rsplit('.').next()?;
                let imported = self.bindings.value_binding(first).is_some();
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

    /// The value node of `key` in an object literal (`{ key: v }` or the shorthand `{ key }`).
    pub(crate) fn object_value<'t>(&self, object: Node<'t>, key: &str) -> Option<Node<'t>> {
        let object = TypeScriptExtractor::peel_wrappers(object);
        if object.kind() != "object" {
            return None;
        }
        object
            .named_children(&mut object.walk())
            .find_map(|member| match member.kind() {
                "pair" => member
                    .child_by_field_name("key")
                    .and_then(|k| member_key(k, self.source))
                    .filter(|k| k == key)
                    .and_then(|_| member.child_by_field_name("value")),
                "shorthand_property_identifier" => {
                    (member.utf8_text(self.source).ok() == Some(key)).then_some(member)
                }
                _ => None,
            })
    }
}
