//! Typed API layers declared in code: tRPC procedures and GraphQL resolvers.
//!
//! Both are recorded as `HttpEndpoint` nodes (an API surface a client calls) named and signed
//! `TRPC query user.get` / `GRAPHQL Query users`:
//! - tRPC: a `router({ … })` property whose value is a procedure chain ending in `.query` /
//!   `.mutation` / `.subscription` (`publicProcedure.input(…).query(…)`), in a file that is tRPC's;
//!   nested routers, inline or through a router variable of the same file, compose the dotted path.
//! - GraphQL: `@Query()` / `@Mutation()` / `@Subscription()` methods of `@nestjs/graphql` and
//!   `type-graphql` resolvers, and the `Query` / `Mutation` / `Subscription` maps of a `resolvers`
//!   object in a file that loads a GraphQL library.

use super::scope::{ModuleBindings, Names};
use super::{arguments, FrameworkCtx};
use std::collections::HashMap;
use tree_sitter::Node;

const GRAPHQL_DECORATOR_LIBS: &[&str] = &["@nestjs/graphql", "type-graphql"];
const GRAPHQL_LIBS: &[&str] = &[
    "graphql",
    "graphql-tag",
    "@apollo/server",
    "@apollo/subgraph",
    "apollo-server",
    "apollo-server-express",
    "apollo-server-core",
    "graphql-yoga",
    "@graphql-yoga/",
    "@graphql-tools/",
    "mercurius",
    "@pothos/",
    "@nestjs/graphql",
    "@nestjs/apollo",
];
const OPERATIONS: &[&str] = &["Query", "Mutation", "Subscription"];

/// `router variable -> (its key in the router that mounts it, that router's variable)`.
pub(super) type RouterMounts = HashMap<String, (String, Option<String>)>;

fn text<'a>(node: Node, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

/// A property or method name: `getUser`, `'get-user'`.
fn key_text(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "property_identifier" | "identifier" | "private_property_identifier" => {
            text(node, source).map(str::to_string)
        }
        "string" => super::scope::unquote(node, source),
        _ => None,
    }
}

/// The identifier a call chain starts from (`publicProcedure` in
/// `publicProcedure.input(z.string()).query(…)`, `procedure` in `t.procedure…`).
fn chain_root_name(mut node: Node, source: &[u8]) -> Option<String> {
    for _ in 0..crate::guard::AstGuard::MAX_NESTING_DEPTH {
        match node.kind() {
            "call_expression" => node = node.child_by_field_name("function")?,
            "member_expression" => {
                let object = node.child_by_field_name("object")?;
                if object.kind() == "identifier" {
                    // `t.procedure`: the procedure is the property.
                    let prop = text(node.child_by_field_name("property")?, source)?;
                    if prop.to_ascii_lowercase().contains("procedure") {
                        return Some(prop.to_string());
                    }
                }
                node = object;
            }
            "identifier" => return text(node, source).map(str::to_string),
            _ => return None,
        }
    }
    None
}

/// The `router({ … })` call whose argument object holds `pair`, and the `pair` mounting that
/// call in a parent router (`user: router({ … })`), if any.
fn enclosing_router<'t>(pair: Node<'t>) -> Option<(Node<'t>, Option<Node<'t>>)> {
    let object = pair.parent().filter(|o| o.kind() == "object")?;
    let arguments_node = object.parent().filter(|p| p.kind() == "arguments")?;
    let call = arguments_node
        .parent()
        .filter(|c| c.kind() == "call_expression")?;
    let mount = call.parent().filter(|p| p.kind() == "pair");
    Some((call, mount))
}

/// The variable a `router({ … })` call is assigned to.
fn router_variable(call: Node, source: &[u8]) -> Option<String> {
    let declarator = call
        .parent()
        .filter(|p| p.kind() == "variable_declarator")?;
    text(declarator.child_by_field_name("name")?, source).map(str::to_string)
}

/// Routers of the file that mount another router *variable* (`post: postRouter`), by variable.
pub(super) fn collect_router_mounts(root: Node, source: &[u8]) -> RouterMounts {
    let mut mounts = RouterMounts::new();
    let mut cursor = root.walk();
    loop {
        let n = cursor.node();
        if n.kind() == "pair" {
            if let (Some(value), Some(key)) = (
                n.child_by_field_name("value")
                    .filter(|v| v.kind() == "identifier"),
                n.child_by_field_name("key")
                    .and_then(|k| key_text(k, source)),
            ) {
                if let (Some(var), Some((call, _))) = (text(value, source), enclosing_router(n)) {
                    mounts
                        .entry(var.to_string())
                        .or_insert((key, router_variable(call, source)));
                }
            }
        }
        if cursor.goto_first_child() || cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return mounts;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// tRPC: the procedure a `key: procedureChain.query(…)` property declares, as
/// `query user.get` with the keys of the enclosing routers.
pub(crate) fn trpc_procedure(pair: Node, source: &[u8], fx: &FrameworkCtx) -> Option<String> {
    let value = pair
        .child_by_field_name("value")
        .filter(|v| v.kind() == "call_expression")?;
    let operation = text(
        value
            .child_by_field_name("function")
            .filter(|f| f.kind() == "member_expression")?
            .child_by_field_name("property")?,
        source,
    )?;
    if !matches!(operation, "query" | "mutation" | "subscription") {
        return None;
    }
    if !fx.mentions_trpc()
        || !chain_root_name(value, source)?
            .to_ascii_lowercase()
            .contains("procedure")
    {
        return None;
    }
    let key = key_text(pair.child_by_field_name("key")?, source)?;
    // Nested routers: `router({ user: router({ get: … }) })` -> `user.get`.
    let mut path = vec![key];
    let mut current = enclosing_router(pair);
    let mut top: Option<Node> = None;
    for _ in 0..crate::guard::AstGuard::MAX_NESTING_DEPTH {
        let Some((call, mount)) = current else { break };
        top = Some(call);
        match mount {
            Some(mount_pair) => {
                let Some(parent_key) = mount_pair
                    .child_by_field_name("key")
                    .and_then(|k| key_text(k, source))
                else {
                    break;
                };
                path.push(parent_key);
                current = enclosing_router(mount_pair);
            }
            None => break,
        }
    }
    path.reverse();
    // The outermost router of the chain may itself be mounted through a variable of the file
    // (`export const postRouter = router({…})` ... `appRouter = router({ post: postRouter })`).
    if let Some(var) = top.and_then(|call| router_variable(call, source)) {
        let mounts = fx.router_mounts();
        let mut outer: Vec<String> = Vec::new();
        let mut current = Some(var);
        let mut seen: Vec<String> = Vec::new();
        while let Some(v) = current.take() {
            if seen.contains(&v) || seen.len() >= 8 {
                break;
            }
            seen.push(v.clone());
            if let Some((key, parent)) = mounts.get(&v) {
                outer.push(key.clone());
                current = parent.clone();
            }
        }
        outer.reverse();
        outer.extend(path);
        path = outer;
    }
    Some(format!("{operation} {}", path.join(".")))
}

/// Whether an object is the `resolvers` of a GraphQL server: declared as `resolvers` /
/// `userResolvers`, or the value of a `resolvers:` option.
fn is_resolvers_object(object: Node, source: &[u8]) -> bool {
    match object.parent() {
        Some(p) if p.kind() == "variable_declarator" => p
            .child_by_field_name("name")
            .and_then(|n| text(n, source))
            .is_some_and(|n| n.to_ascii_lowercase().contains("resolver")),
        Some(p) if p.kind() == "pair" => p
            .child_by_field_name("key")
            .and_then(|k| key_text(k, source))
            .is_some_and(|k| k == "resolvers"),
        _ => false,
    }
}

/// GraphQL resolvers of a `resolvers` map: for `Query: { users() {}, user: () => … }` the labels
/// `Query users`, `Query user` with the lines of each field, at most `budget` of them.
pub(crate) fn graphql_map_fields(
    pair: Node,
    source: &[u8],
    fx: &FrameworkCtx,
    budget: usize,
) -> Vec<(String, (usize, usize))> {
    let Some(object) = pair
        .child_by_field_name("value")
        .filter(|v| v.kind() == "object")
    else {
        return Vec::new();
    };
    let Some(key) = pair
        .child_by_field_name("key")
        .and_then(|k| key_text(k, source))
        .filter(|k| OPERATIONS.contains(&k.as_str()))
    else {
        return Vec::new();
    };
    let owner_is_resolvers = pair
        .parent()
        .filter(|o| o.kind() == "object")
        .is_some_and(|o| is_resolvers_object(o, source));
    if !owner_is_resolvers
        || !fx.mentions_graphql()
        || !fx.built().bindings.references_any(GRAPHQL_LIBS)
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    for member in object.named_children(&mut object.walk()).take(budget) {
        let name = match member.kind() {
            "pair" => member
                .child_by_field_name("key")
                .and_then(|k| key_text(k, source)),
            "method_definition" => member
                .child_by_field_name("name")
                .and_then(|k| key_text(k, source)),
            "shorthand_property_identifier" => text(member, source).map(str::to_string),
            _ => None,
        };
        if let Some(name) = name {
            out.push((
                format!("{key} {name}"),
                (
                    member.start_position().row + 1,
                    member.end_position().row + 1,
                ),
            ));
        }
    }
    out
}

/// GraphQL: the operation a `@Query()` / `@Mutation()` / `@Subscription()` method of a
/// `@nestjs/graphql` or `type-graphql` resolver serves, as `Query users`: the `name` option of the
/// decorator, a schema-first name (`@Query('author')`), else the method's name.
pub(crate) fn graphql_decorator(method: Node, source: &[u8], fx: &FrameworkCtx) -> Option<String> {
    if !fx.mentions_graphql_decorators() {
        return None;
    }
    let mut decorators: Vec<Node> = method
        .children(&mut method.walk())
        .filter(|c| c.kind() == "decorator")
        .collect();
    let mut prev = method.prev_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "decorator" => decorators.push(p),
            "comment" => {}
            _ => break,
        }
        prev = p.prev_sibling();
    }
    let built = fx.built();
    for decorator in decorators {
        let Some(call) = decorator.named_child(0) else {
            continue;
        };
        let function = if call.kind() == "call_expression" {
            match call.child_by_field_name("function") {
                Some(f) => f,
                None => continue,
            }
        } else {
            call
        };
        let Some(local) = text(function, source) else {
            continue;
        };
        // Only the GraphQL libraries' decorator (also through an alias: `Query as Q`), never
        // `@Query()` of the REST `@nestjs/common`.
        let Some(binding) = built.bindings.value_binding(local) else {
            continue;
        };
        if !OPERATIONS.contains(&binding.imported.as_str())
            || !ModuleBindings::module_in(&binding.module, GRAPHQL_DECORATOR_LIBS)
        {
            continue;
        }
        let names = Names {
            source,
            consts: &built.consts,
            bindings: &built.bindings,
            scopes: &built.scopes,
        };
        let args = if call.kind() == "call_expression" {
            arguments(call)
        } else {
            Vec::new()
        };
        let custom = args
            .iter()
            .find_map(|a| names.object_value(*a, "name"))
            .and_then(|v| names.resolve(v))
            // Schema-first: `@Query('author')`.
            .or_else(|| {
                args.first()
                    .filter(|a| a.kind() == "string")
                    .and_then(|a| names.resolve(*a))
            });
        let name = custom.or_else(|| {
            method
                .child_by_field_name("name")
                .and_then(|n| key_text(n, source))
        })?;
        return Some(format!("{} {name}", binding.imported));
    }
    None
}
