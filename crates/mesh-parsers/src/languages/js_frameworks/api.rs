//! Typed API layers declared in code: tRPC procedures and GraphQL resolvers.
//!
//! Both are recorded as `HttpEndpoint` nodes (an API surface a client calls) named and signed
//! `TRPC query user.get` / `GRAPHQL Query users`:
//! - tRPC: a `router({ … })` property whose value is a procedure chain ending in `.query` /
//!   `.mutation` / `.subscription` (`publicProcedure.input(…).query(…)`), nested routers
//!   composing the dotted path.
//! - GraphQL: `@Query()` / `@Mutation()` / `@Subscription()` methods of `@nestjs/graphql` and
//!   `type-graphql` resolvers, and the `Query` / `Mutation` / `Subscription` maps of an
//!   Apollo-style `resolvers` object.

use super::scope::Names;
use super::{arguments, FrameworkCtx};
use tree_sitter::Node;

const GRAPHQL_DECORATOR_LIBS: &[&str] = &["@nestjs/graphql", "type-graphql"];
const GRAPHQL_FILE_MENTIONS: &[&str] = &[
    "graphql",
    "apollo",
    "gql`",
    "gql(",
    "mercurius",
    "yoga",
    "pothos",
];
const OPERATIONS: &[&str] = &["Query", "Mutation", "Subscription"];

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

/// tRPC: the procedure a `key: procedureChain.query(…)` property declares, as
/// `("query", "user.get")` with the keys of the enclosing routers.
pub(crate) fn trpc_procedure(pair: Node, source: &[u8]) -> Option<String> {
    let key = key_text(pair.child_by_field_name("key")?, source)?;
    let value = pair.child_by_field_name("value")?;
    if value.kind() != "call_expression" {
        return None;
    }
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
    if !chain_root_name(value, source)?
        .to_ascii_lowercase()
        .contains("procedure")
    {
        return None;
    }
    // Nested routers: `router({ user: router({ get: … }) })` -> `user.get`.
    let mut path = vec![key];
    let mut current = pair.parent();
    for _ in 0..crate::guard::AstGuard::MAX_NESTING_DEPTH {
        let Some(object) = current.filter(|o| o.kind() == "object") else {
            break;
        };
        let Some(arguments_node) = object.parent().filter(|p| p.kind() == "arguments") else {
            break;
        };
        let Some(call) = arguments_node
            .parent()
            .filter(|c| c.kind() == "call_expression")
        else {
            break;
        };
        let router_pair = call.parent().filter(|p| p.kind() == "pair");
        match router_pair
            .and_then(|p| p.child_by_field_name("key"))
            .and_then(|k| key_text(k, source))
        {
            Some(parent_key) => {
                path.push(parent_key);
                current = router_pair.and_then(|p| p.parent());
            }
            None => break,
        }
    }
    path.reverse();
    Some(format!("{operation} {}", path.join(".")))
}

/// GraphQL resolvers of an Apollo-style map: for `Query: { users() {}, user: () => … }` the
/// labels `Query users`, `Query user` with the lines of each field.
pub(crate) fn graphql_map_fields(
    pair: Node,
    source: &[u8],
    fx: &FrameworkCtx,
) -> Vec<(String, (usize, usize))> {
    let Some(key) = pair
        .child_by_field_name("key")
        .and_then(|k| key_text(k, source))
    else {
        return Vec::new();
    };
    if !OPERATIONS.contains(&key.as_str()) {
        return Vec::new();
    }
    let Some(object) = pair
        .child_by_field_name("value")
        .filter(|v| v.kind() == "object")
    else {
        return Vec::new();
    };
    if !fx.mentions_any(GRAPHQL_FILE_MENTIONS) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for member in object.named_children(&mut object.walk()) {
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
/// `@nestjs/graphql` or `type-graphql` resolver serves, as `Query users` (the `name` option
/// of the decorator, else the method's name).
pub(crate) fn graphql_decorator(method: Node, source: &[u8], fx: &FrameworkCtx) -> Option<String> {
    if !fx.mentions_any(&["@nestjs/graphql", "type-graphql"]) {
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
        let call = decorator.named_child(0)?;
        let function = if call.kind() == "call_expression" {
            call.child_by_field_name("function")?
        } else {
            call
        };
        let local = text(function, source)?;
        if !OPERATIONS.contains(&local) {
            continue;
        }
        // Only the GraphQL libraries' `Query`, not `@Query()` of the REST `@nestjs/common`.
        let binding = built.bindings.value_binding(local)?;
        if !super::scope::ModuleBindings::module_in(&binding.module, GRAPHQL_DECORATOR_LIBS) {
            continue;
        }
        let names = Names {
            source,
            consts: &built.consts,
            bindings: &built.bindings,
            scopes: &built.scopes,
        };
        let custom = if call.kind() == "call_expression" {
            arguments(call)
                .iter()
                .find_map(|a| names.object_value(*a, "name"))
                .and_then(|v| names.resolve(v))
        } else {
            None
        };
        let name = custom.or_else(|| {
            method
                .child_by_field_name("name")
                .and_then(|n| key_text(n, source))
        })?;
        return Some(format!("{local} {name}"));
    }
    None
}
