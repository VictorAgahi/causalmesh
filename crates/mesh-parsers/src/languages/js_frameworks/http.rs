//! HTTP routes as JavaScript / TypeScript frameworks declare them.
//!
//! Two shapes: *registrations* (`app.get('/users/:id', handler)`, `router.route('/p')
//! .post(h)`, `fastify.route({ method, url })`, `server.route({ method, path })`,
//! `app.on('GET', '/p', h)`, with prefixes from `app.group('/api', …)`,
//! `fastify.register(plugin, { prefix })` and in-file `app.use('/api', router)`
//! mounts) and *decorated controllers* (`@Controller('users')` + `@Get(':id')` in
//! NestJS, routing-controllers, tsoa, inversify-express-utils).
//!
//! A registration needs the file to load an HTTP framework and, when the file builds
//! apps or routers of it (`express()`, `Router()`, `new Hono()`), the receiver to be
//! one of them: `axios.get('/users', config)` and `res.get('Content-Type')` are not routes.

use super::scope::{receiver_key, Names};
use super::{arguments, FrameworkCtx};
use std::collections::HashMap;
use tree_sitter::Node;

const EXPRESS: &[&str] = &["express"];
const FASTIFY: &[&str] = &["fastify"];
const KOA: &[&str] = &["@koa/router", "koa-router"];
const HONO: &[&str] = &["hono"];
const ELYSIA: &[&str] = &["elysia"];
const H3: &[&str] = &["h3"];
const RESTIFY: &[&str] = &["restify"];
const POLKA: &[&str] = &["polka"];
const ITTY: &[&str] = &["itty-router"];
const HAPI: &[&str] = &["@hapi/hapi", "hapi"];

/// HTTP framework groups whose apps and routers are tracked as receivers.
pub(super) const FAMILIES: &[&[&str]] = &[
    EXPRESS, FASTIFY, KOA, HONO, ELYSIA, H3, RESTIFY, POLKA, ITTY, HAPI,
];

/// Libraries that make requests rather than serve them.
const HTTP_CLIENTS: &[&str] = &[
    "axios",
    "got",
    "ky",
    "superagent",
    "supertest",
    "node-fetch",
    "undici",
    "ofetch",
    "needle",
    "request",
    "@angular/common/http",
];

/// Verb methods of a router (`del` is restify's `delete`).
const VERBS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "del", "head", "options", "all", "any",
];

/// Decorators that declare a route handler, with the HTTP method they stand for.
const VERB_DECORATORS: &[(&str, &str)] = &[
    ("Get", "GET"),
    ("Post", "POST"),
    ("Put", "PUT"),
    ("Patch", "PATCH"),
    ("Delete", "DELETE"),
    ("Head", "HEAD"),
    ("Options", "OPTIONS"),
    ("All", "ALL"),
    ("httpGet", "GET"),
    ("httpPost", "POST"),
    ("httpPut", "PUT"),
    ("httpPatch", "PATCH"),
    ("httpDelete", "DELETE"),
    ("httpHead", "HEAD"),
    ("httpOptions", "OPTIONS"),
    ("httpAll", "ALL"),
];

/// Class decorators that give a controller its path prefix.
const CONTROLLER_DECORATORS: &[&str] = &["Controller", "JsonController", "Route", "controller"];

/// A route found at a call site or on a decorated method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteFact {
    /// `GET`, `POST`, …, `ALL`.
    pub method: String,
    /// The full path, prefixes included (`/api/users/:id`).
    pub path: String,
}

impl RouteFact {
    pub(crate) fn label(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// Whether `source` could declare an HTTP route at all: it names an HTTP framework or
/// uses a route decorator (`@Get`, `@httpPost`, …).
pub(super) fn mentions_http(source: &str) -> bool {
    FAMILIES
        .iter()
        .flat_map(|libs| libs.iter())
        .any(|lib| source.contains(lib))
        || (source.contains('@')
            && VERB_DECORATORS
                .iter()
                .any(|(d, _)| source.contains(&format!("@{d}"))))
}

/// Joins path segments with single slashes: `("/api/", "/users")` -> `/api/users`.
fn join(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    let joined = match (prefix.is_empty(), path.is_empty()) {
        (true, true) => String::new(),
        (true, false) if path.starts_with('*') => path.to_string(),
        (true, false) => format!("/{path}"),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{prefix}/{path}"),
    };
    let joined = if joined.starts_with('/') || joined.starts_with('*') {
        joined
    } else {
        format!("/{joined}")
    };
    // `/users/` and `/users` are one route.
    if joined.len() > 1 {
        joined.trim_end_matches('/').to_string()
    } else {
        joined
    }
}

fn is_handler(node: Node) -> bool {
    matches!(
        node.kind(),
        "arrow_function"
            | "function_expression"
            | "generator_function"
            | "identifier"
            | "member_expression"
            | "call_expression"
            | "await_expression"
    )
}

fn method_name<'a>(call: Node, source: &'a [u8]) -> Option<&'a str> {
    let callee = call.child_by_field_name("function")?;
    if callee.kind() != "member_expression" {
        return None;
    }
    callee
        .child_by_field_name("property")?
        .utf8_text(source)
        .ok()
}

/// Paths a registration's first argument stands for: a path, or an array of them.
fn route_paths(names: &Names, node: Node) -> Vec<String> {
    names
        .resolve_all(node)
        .into_iter()
        .filter(|p| p.starts_with('/') || p.starts_with('*') || p.is_empty())
        .collect()
}

/// Upper-cased HTTP methods named by a string or an array of strings.
fn methods_of(names: &Names, node: Node) -> Vec<String> {
    names
        .resolve_all(node)
        .into_iter()
        .map(|m| m.to_ascii_uppercase())
        .filter(|m| {
            matches!(
                m.as_str(),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" | "ALL"
            )
        })
        .collect()
}

/// An enclosing routing scope of a call: the prefix it adds and the name of the
/// router the callback receives (`app.group('/api', (api) => api.get(…))`).
struct Scope {
    prefix: String,
    router_param: Option<String>,
}

/// The `group('/p', cb)` / `register(cb, { prefix })` scopes around `call`, innermost last.
fn enclosing_scopes(call: Node, source: &[u8], names: &Names) -> Vec<Scope> {
    let mut scopes = Vec::new();
    let mut ancestor = call.parent();
    let mut hops = 0;
    while let Some(a) = ancestor {
        hops += 1;
        if hops > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            break;
        }
        if matches!(a.kind(), "arrow_function" | "function_expression") {
            if let Some(outer) = a
                .parent()
                .filter(|p| p.kind() == "arguments")
                .and_then(|args| args.parent())
                .filter(|c| c.kind() == "call_expression")
            {
                let args = arguments(outer);
                let prefix = match method_name(outer, source) {
                    Some("group") => args.first().and_then(|n| names.resolve(*n)),
                    Some("register") => args
                        .get(1)
                        .and_then(|o| names.object_value(*o, "prefix"))
                        .and_then(|v| names.resolve(v)),
                    _ => None,
                };
                if let Some(prefix) = prefix {
                    let param = a
                        .child_by_field_name("parameters")
                        .and_then(|p| p.named_child(0))
                        .or_else(|| a.child_by_field_name("parameter"))
                        .and_then(|p| {
                            if p.kind() == "identifier" {
                                p.utf8_text(source).ok()
                            } else {
                                p.child_by_field_name("pattern")
                                    .and_then(|i| i.utf8_text(source).ok())
                            }
                        })
                        .map(str::to_string);
                    scopes.push(Scope {
                        prefix,
                        router_param: param,
                    });
                }
            }
        }
        ancestor = a.parent();
    }
    scopes.reverse();
    scopes
}

/// `router -> (prefix, parent router)` for in-file `parent.use('/prefix', router)` mounts.
pub(super) type Mounts = HashMap<String, (String, Option<String>)>;

pub(super) fn collect_mounts(root: Node, source: &[u8], names: &Names) -> Mounts {
    let mut mounts = Mounts::new();
    let mut cursor = root.walk();
    loop {
        let n = cursor.node();
        // `const api = new Router({ prefix: '/api' })` / `new Elysia({ prefix: '/api' })`
        // and `const api = app.basePath('/api')`.
        if n.kind() == "variable_declarator" {
            if let (Some(key), Some(value)) = (
                n.child_by_field_name("name")
                    .filter(|k| k.kind() == "identifier")
                    .and_then(|k| k.utf8_text(source).ok()),
                n.child_by_field_name("value")
                    .map(crate::languages::typescript::TypeScriptExtractor::peel_wrappers),
            ) {
                let prefix = match value.kind() {
                    "new_expression" => arguments(value)
                        .first()
                        .and_then(|o| names.object_value(*o, "prefix"))
                        .and_then(|v| names.resolve(v)),
                    "call_expression" if method_name(value, source) == Some("basePath") => {
                        arguments(value).first().and_then(|a| names.resolve(*a))
                    }
                    _ => None,
                };
                if let Some(prefix) = prefix {
                    let parent = (value.kind() == "call_expression")
                        .then(|| {
                            value
                                .child_by_field_name("function")
                                .and_then(|f| f.child_by_field_name("object"))
                                .and_then(|o| receiver_key(o, source))
                        })
                        .flatten();
                    mounts.entry(key.to_string()).or_insert((prefix, parent));
                }
            }
        }
        // `parent.use('/p', router)` (express, koa, polka, h3) and hono's `parent.route('/p', router)`.
        if n.kind() == "call_expression" && matches!(method_name(n, source), Some("use" | "route"))
        {
            let args = arguments(n);
            let prefix = args.first().and_then(|a| names.resolve(*a));
            if let (Some(prefix), Some(last)) = (prefix, args.last()) {
                if args.len() >= 2 && matches!(last.kind(), "identifier" | "member_expression") {
                    if let Some(router) = receiver_key(*last, source) {
                        let parent = n
                            .child_by_field_name("function")
                            .and_then(|f| f.child_by_field_name("object"))
                            .and_then(|o| receiver_key(o, source));
                        mounts.entry(router).or_insert((prefix, parent));
                    }
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

/// The prefix a router is mounted under, through nested in-file mounts.
fn mounted_prefix(mounts: &Mounts, router: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut current = Some(router);
    for _ in 0..8 {
        let Some((prefix, parent)) = current.and_then(|r| mounts.get(r)) else {
            break;
        };
        parts.push(prefix);
        current = parent.as_deref();
    }
    parts
        .iter()
        .rev()
        .fold(String::new(), |acc, p| join(&acc, p))
}

/// The `route('/p')` call at the base of a `route('/p').get(h).post(h)` chain.
fn chain_base_path<'t>(
    callee: Node<'t>,
    source: &[u8],
    names: &Names,
) -> Option<(String, Node<'t>)> {
    let mut object = callee.child_by_field_name("object")?;
    for _ in 0..16 {
        if object.kind() != "call_expression" {
            return None;
        }
        let method = method_name(object, source)?;
        if method == "route" {
            let path = arguments(object)
                .first()
                .and_then(|a| names.resolve(*a))
                .filter(|p| p.starts_with('/') || p.starts_with('*'))?;
            let base = object.child_by_field_name("function")?;
            return Some((path, base));
        }
        if !VERBS.contains(&method) {
            return None;
        }
        object = object
            .child_by_field_name("function")?
            .child_by_field_name("object")?;
    }
    None
}

/// Route registrations at one call expression.
pub(crate) fn detect_routes(call: Node, source: &[u8], fx: &FrameworkCtx) -> Vec<RouteFact> {
    let Some(method) = method_name(call, source) else {
        return Vec::new();
    };
    let is_verb = VERBS.contains(&method);
    if !(is_verb || matches!(method, "route" | "on")) || !fx.http_relevant() {
        return Vec::new();
    }
    let built = fx.built();
    let all_libs: Vec<&str> = FAMILIES.iter().flat_map(|l| l.iter().copied()).collect();
    if !built.bindings.imports_any(&all_libs) {
        return Vec::new();
    }
    let names = Names {
        source,
        consts: &built.consts,
        bindings: &built.bindings,
        scopes: &built.scopes,
    };
    let Some(callee) = call.child_by_field_name("function") else {
        return Vec::new();
    };
    let args = arguments(call);
    let scopes = enclosing_scopes(call, source, &names);

    // Which framework's app/router this is called on.
    let base = chain_base_path(callee, source, &names);
    let receiver = match &base {
        Some((_, route_call_callee)) => route_call_callee.child_by_field_name("object"),
        None => callee.child_by_field_name("object"),
    };
    let key = receiver.and_then(|r| receiver_key(r, source));
    let in_group_callback = key
        .as_deref()
        .is_some_and(|k| scopes.iter().any(|s| s.router_param.as_deref() == Some(k)));
    let imported: Vec<&&[&str]> = FAMILIES
        .iter()
        .filter(|libs| built.bindings.imports_any(libs))
        .collect();
    // `axios.get('/users', config)` is a client call even in a file that also serves.
    let is_http_client = key.as_deref().is_some_and(|k| {
        built
            .bindings
            .value_binding(k)
            .is_some_and(|b| super::scope::ModuleBindings::module_in(&b.module, HTTP_CLIENTS))
    });
    if is_http_client {
        return Vec::new();
    }
    let is_router = in_group_callback
        || imported.iter().any(|libs| {
            built
                .receivers
                .is_client(libs[0], key.as_deref().unwrap_or(""))
                .unwrap_or(true)
        });
    if !is_router {
        return Vec::new();
    }

    let mut prefix = String::new();
    if let Some(k) = key.as_deref() {
        prefix = mounted_prefix(&fx.mounts(&names), k);
    }
    for scope in &scopes {
        prefix = join(&prefix, &scope.prefix);
    }

    let mut routes: Vec<(String, String)> = Vec::new(); // (method, path)
    match method {
        _ if is_verb => {
            let verb = match method {
                "del" => "DELETE".to_string(),
                "any" => "ALL".to_string(),
                m => m.to_ascii_uppercase(),
            };
            if let Some((path, _)) = &base {
                // `router.route('/p').get(handler)`
                if args.last().is_some_and(|h| is_handler(*h)) {
                    routes.push((verb, path.clone()));
                }
            } else if args.len() >= 2 && args.last().is_some_and(|h| is_handler(*h)) {
                for path in route_paths(&names, args[0]) {
                    routes.push((verb.clone(), path));
                }
            }
        }
        // `fastify.route({ method, url, handler })` / `server.route({ method, path, handler })`
        "route" => {
            let objects: Vec<Node> = match args.first() {
                Some(a) if a.kind() == "object" => vec![*a],
                Some(a) if a.kind() == "array" => a.named_children(&mut a.walk()).collect(),
                _ => Vec::new(),
            };
            for object in objects {
                let path = ["url", "path"]
                    .iter()
                    .find_map(|k| names.object_value(object, k))
                    .and_then(|v| names.resolve(v));
                let methods = names
                    .object_value(object, "method")
                    .map(|v| methods_of(&names, v))
                    .unwrap_or_else(|| vec!["ALL".to_string()]);
                if let Some(path) = path.filter(|p| p.starts_with('/') || p.starts_with('*')) {
                    for m in methods {
                        routes.push((m, path.clone()));
                    }
                }
            }
        }
        // hono / h3: `app.on('GET', '/p', handler)`, `app.on(['GET', 'POST'], '/p', h)`
        "on" => {
            if args.len() >= 3 && args.last().is_some_and(|h| is_handler(*h)) {
                let methods = methods_of(&names, args[0]);
                for path in route_paths(&names, args[1]) {
                    for m in &methods {
                        routes.push((m.clone(), path.clone()));
                    }
                }
            }
        }
        _ => {}
    }
    routes
        .into_iter()
        .map(|(method, path)| RouteFact {
            method,
            path: join(&prefix, &path),
        })
        .collect()
}

/// The decorators attached to a class member or class, whether they are children of
/// it or siblings preceding it.
fn decorators_of(node: Node) -> Vec<Node> {
    let mut out: Vec<Node> = node
        .children(&mut node.walk())
        .filter(|c| c.kind() == "decorator")
        .collect();
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "decorator" => out.push(p),
            "comment" => {}
            _ => break,
        }
        prev = p.prev_sibling();
    }
    out
}

/// The path argument of a decorator: `@Get(':id')`, `@Controller({ path: 'users' })`,
/// `@Get()` (empty).
fn decorator_path(decorator: Node, names: &Names) -> Option<String> {
    let call = decorator.named_child(0)?;
    let (name, args) = match call.kind() {
        "call_expression" => (call.child_by_field_name("function")?, arguments(call)),
        "identifier" => (call, Vec::new()),
        _ => return None,
    };
    let _ = name;
    match args.first() {
        None => Some(String::new()),
        Some(arg) => {
            let arg = *arg;
            if arg.kind() == "object" {
                names
                    .object_value(arg, "path")
                    .and_then(|v| names.resolve(v))
                    .or_else(|| Some(String::new()))
            } else {
                names.resolve_all(arg).into_iter().next()
            }
        }
    }
}

fn decorator_name<'a>(decorator: Node, source: &'a [u8]) -> Option<&'a str> {
    let call = decorator.named_child(0)?;
    let function = if call.kind() == "call_expression" {
        call.child_by_field_name("function")?
    } else {
        call
    };
    function.utf8_text(source).ok()
}

/// The route a decorated controller method serves: the controller prefix
/// (`@Controller('users')`) joined with the verb decorator's path (`@Get(':id')`).
pub(crate) fn decorator_route(method: Node, source: &[u8], fx: &FrameworkCtx) -> Option<RouteFact> {
    let verb_decorator = decorators_of(method).into_iter().find_map(|d| {
        let name = decorator_name(d, source)?;
        VERB_DECORATORS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, verb)| (d, *verb))
    })?;
    let built = fx.built();
    let names = Names {
        source,
        consts: &built.consts,
        bindings: &built.bindings,
        scopes: &built.scopes,
    };
    let own = decorator_path(verb_decorator.0, &names)?;
    let mut prefix = String::new();
    let mut ancestor = method.parent();
    while let Some(a) = ancestor {
        if matches!(
            a.kind(),
            "class_declaration" | "abstract_class_declaration" | "class"
        ) {
            // `@Controller('x') export class C {}` keeps its decorators on the `export`.
            let mut candidates = decorators_of(a);
            if let Some(export) = a.parent().filter(|p| p.kind() == "export_statement") {
                candidates.extend(decorators_of(export));
            }
            if let Some(controller) = candidates.into_iter().find(|d| {
                decorator_name(*d, source).is_some_and(|n| CONTROLLER_DECORATORS.contains(&n))
            }) {
                prefix = decorator_path(controller, &names).unwrap_or_default();
            }
            break;
        }
        ancestor = a.parent();
    }
    Some(RouteFact {
        method: verb_decorator.1.to_string(),
        path: join(&prefix, &own),
    })
}
