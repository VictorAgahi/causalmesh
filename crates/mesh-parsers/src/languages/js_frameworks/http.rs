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
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
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

/// Modules whose route decorators these are; a verb-named decorator imported from any
/// other module (`@Get` of a config or ORM library) declares no route.
const DECORATOR_LIBS: &[&str] = &[
    "@nestjs/common",
    "routing-controllers",
    "tsoa",
    "inversify-express-utils",
    "@tsed/schema",
    "@tsed/common",
    "@overnightjs/core",
    "@decorators/express",
    "@midwayjs/core",
    "@midwayjs/decorator",
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

/// Whether `source` names `lib` as a module: delimited by a quote on both sides or a `/`
/// after (`'hono'`, `"hono/http-exception"`), not as a substring of "honor" or `<h3>`.
fn mentions_module(source: &str, lib: &str) -> bool {
    source.match_indices(lib).any(|(at, _)| {
        let before = source[..at].chars().next_back();
        let after = source[at + lib.len()..].chars().next();
        matches!(before, Some('\'' | '"' | '`')) && matches!(after, Some('\'' | '"' | '`' | '/'))
    })
}

/// Whether `source` could declare an HTTP route at all: it names an HTTP framework as a
/// module or uses a route decorator (`@Get`, `@httpPost`, …).
pub(super) fn mentions_http(source: &str) -> bool {
    FAMILIES
        .iter()
        .flat_map(|libs| libs.iter())
        .any(|lib| mentions_module(source, lib))
        || (source.contains('@')
            && VERB_DECORATORS
                .iter()
                .any(|(d, _)| source.contains(&format!("@{d}"))))
}

/// Whether a name looks like an app or router when nothing in the file says so: its last
/// word (`usersRouter`, `api_routes`, `app`) names one. `application`, `routeDecorator`,
/// `appConfig`, `mapper` and `cache` do not.
fn looks_like_router(key: &str) -> bool {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in key.chars() {
        if c == '_' || c == '-' || c == '$' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else if c.is_ascii_uppercase() && !current.is_empty() {
            words.push(std::mem::take(&mut current));
            current.push(c.to_ascii_lowercase());
        } else {
            current.push(c.to_ascii_lowercase());
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words.last().is_some_and(|w| {
        matches!(
            w.as_str(),
            "app"
                | "router"
                | "routers"
                | "route"
                | "routes"
                | "server"
                | "fastify"
                | "hono"
                | "elysia"
                | "koa"
        )
    })
}

/// The framework constructor a method chain starts from (`new Elysia(…)`, `Router()`,
/// `fastify()`, `express.Router()`), when it is one of an imported HTTP framework.
fn framework_ctor_root<'t>(
    receiver: Node<'t>,
    source: &[u8],
    imported: &[&&[&str]],
    built: &super::Built,
) -> Option<Node<'t>> {
    let mut current = receiver;
    for _ in 0..crate::guard::AstGuard::MAX_NESTING_DEPTH {
        match current.kind() {
            "call_expression" => {
                let function = current.child_by_field_name("function")?;
                if function.kind() == "member_expression" {
                    // `express.Router()` is itself the constructor call.
                    if is_framework_name(function, source, imported, built) {
                        return Some(current);
                    }
                    current = function.child_by_field_name("object")?;
                    continue;
                }
                return is_framework_name(function, source, imported, built).then_some(current);
            }
            "new_expression" => {
                let ctor = current.child_by_field_name("constructor")?;
                return is_framework_name(ctor, source, imported, built).then_some(current);
            }
            "member_expression" => current = current.child_by_field_name("object")?,
            "await_expression" | "parenthesized_expression" => current = current.named_child(0)?,
            _ => return None,
        }
    }
    None
}

/// Whether `name` (an identifier or `ns.Name`) is bound to an imported HTTP framework.
fn is_framework_name(
    name: Node,
    source: &[u8],
    imported: &[&&[&str]],
    built: &super::Built,
) -> bool {
    let root = match name.kind() {
        "member_expression" => name
            .child_by_field_name("object")
            .filter(|o| o.kind() == "identifier"),
        _ => Some(name),
    };
    root.and_then(|r| r.utf8_text(source).ok())
        .and_then(|local| built.bindings.value_binding(local))
        .is_some_and(|b| {
            imported
                .iter()
                .any(|libs| super::scope::ModuleBindings::module_in(&b.module, libs))
        })
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
        // hapi's `'*'` is every method.
        .map(|m| if m == "*" { "ALL".to_string() } else { m })
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

/// One mount of a router: the prefix it is mounted under, the router it is mounted on,
/// and the function it happens in (a router variable name is reused across functions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mount {
    prefix: String,
    parent: Option<String>,
    scope: (usize, usize),
}

/// `router -> its mounts` for in-file `parent.use('/prefix', router)` mounts,
/// `new Router({ prefix })` and `app.basePath('/p')` declarations.
pub(super) type Mounts = HashMap<String, Vec<Mount>>;

/// The byte range of the function around `node`, or the whole file.
fn function_scope(node: Node) -> (usize, usize) {
    let mut ancestor = node.parent();
    for _ in 0..crate::guard::AstGuard::MAX_NESTING_DEPTH {
        let Some(a) = ancestor else { break };
        if matches!(
            a.kind(),
            "function_declaration"
                | "function_expression"
                | "generator_function"
                | "generator_function_declaration"
                | "arrow_function"
                | "method_definition"
        ) {
            return (a.start_byte(), a.end_byte());
        }
        ancestor = a.parent();
    }
    (0, usize::MAX)
}

/// Mounts kept per router: an identical mount is recorded once, and a router mounted more
/// often than this is truncated (declaration order).
const MAX_MOUNTS_PER_ROUTER: usize = 64;

fn add_mount(mounts: &mut Mounts, router: &str, mount: Mount) {
    let entries = mounts.entry(router.to_string()).or_default();
    if entries.len() < MAX_MOUNTS_PER_ROUTER && !entries.contains(&mount) {
        entries.push(mount);
    }
}

/// `(router, function scope)` -> the prefixes computed for it, per file.
pub(super) type PrefixCache = RefCell<HashMap<(String, Option<(usize, usize)>), Rc<Vec<String>>>>;

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
                    add_mount(
                        &mut mounts,
                        key,
                        Mount {
                            prefix,
                            parent,
                            scope: function_scope(n),
                        },
                    );
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
                        add_mount(
                            &mut mounts,
                            &router,
                            Mount {
                                prefix,
                                parent,
                                scope: function_scope(n),
                            },
                        );
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

/// Prefixes kept per router: a router mounted under more is truncated, deterministically
/// (declaration order), rather than fanned out without bound.
const MAX_PREFIXES: usize = 4;

/// Every prefix a router is mounted under, through nested in-file mounts. At the first
/// level only the mounts made in the function of the use site count (a name reused
/// in two functions is two routers); with none there, all of them do. A router mounted
/// twice has two prefixes. Bounded in depth and in number, and a mount cycle stops.
fn mounted_prefixes(
    mounts: &Mounts,
    cache: &PrefixCache,
    router: &str,
    scope: (usize, usize),
) -> Rc<Vec<String>> {
    fn go(
        mounts: &Mounts,
        cache: &PrefixCache,
        router: &str,
        scope: Option<(usize, usize)>,
        path: &mut Vec<String>,
    ) -> Rc<Vec<String>> {
        let key = (router.to_string(), scope);
        if let Some(hit) = cache.borrow().get(&key) {
            return Rc::clone(hit);
        }
        let Some(entries) = mounts.get(router).filter(|_| path.len() < 8) else {
            return Rc::new(vec![String::new()]);
        };
        if path.iter().any(|r| r == router) {
            return Rc::new(vec![String::new()]);
        }
        let local: Vec<&Mount> = entries
            .iter()
            .filter(|m| scope.is_none_or(|s| m.scope == s))
            .collect();
        let chosen: Vec<&Mount> = if local.is_empty() {
            entries.iter().collect()
        } else {
            local
        };
        path.push(router.to_string());
        let mut out: Vec<String> = Vec::new();
        'mounts: for mount in chosen {
            let parents = match &mount.parent {
                Some(parent) => go(mounts, cache, parent, None, path),
                None => Rc::new(vec![String::new()]),
            };
            for parent_prefix in parents.iter() {
                let prefix = join(parent_prefix, &mount.prefix);
                if !out.contains(&prefix) {
                    out.push(prefix);
                }
                // The cap bounds the whole fan-out, not just one level of it.
                if out.len() >= MAX_PREFIXES {
                    break 'mounts;
                }
            }
        }
        path.pop();
        let out = Rc::new(out);
        cache.borrow_mut().insert(key, Rc::clone(&out));
        out
    }
    go(mounts, cache, router, Some(scope), &mut Vec::new())
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
    if !FAMILIES.iter().any(|libs| built.bindings.imports_any(libs)) {
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
    // `new Elysia({ prefix }).get(…).post(…)`, `Router().get(…)`, `fastify().get(…)`: the
    // chain starts at a framework constructor, whatever the file also builds.
    let ctor_root = receiver.and_then(|r| framework_ctor_root(r, source, &imported, built));
    let key_name = key.as_deref().unwrap_or("");
    let verdicts: Vec<Option<bool>> = imported
        .iter()
        .map(|libs| built.receivers.is_client(libs[0], key_name))
        .collect();
    let is_router = in_group_callback
        || ctor_root.is_some()
        || verdicts.contains(&Some(true))
        // The file builds no app or router of its own (it takes one as an argument, or
        // imports it): the receiver must at least look like one.
        || (verdicts.iter().all(Option::is_none) && looks_like_router(key_name));
    if !is_router {
        return Vec::new();
    }

    let mut prefixes = vec![String::new()];
    if let Some(k) = key.as_deref() {
        prefixes = mounted_prefixes(
            fx.mounts(&names),
            fx.prefix_cache(),
            k,
            function_scope(call),
        )
        .to_vec();
    }
    if let Some(root) = ctor_root.filter(|r| r.kind() == "new_expression") {
        if let Some(prefix) = arguments(root)
            .first()
            .and_then(|o| names.object_value(*o, "prefix"))
            .and_then(|v| names.resolve(v))
        {
            prefixes = prefixes.iter().map(|p| join(p, &prefix)).collect();
        }
    }
    for scope in &scopes {
        prefixes = prefixes.iter().map(|p| join(p, &scope.prefix)).collect();
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
                let mut paths = route_paths(&names, args[0]);
                // koa-router named route: `router.get('user', '/users/:id', handler)`.
                if paths.is_empty() && args.len() >= 3 && names.resolve(args[0]).is_some() {
                    paths = route_paths(&names, args[1]);
                }
                for path in paths {
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
        "on" if args.len() >= 3 && args.last().is_some_and(|h| is_handler(*h)) => {
            let methods = methods_of(&names, args[0]);
            for path in route_paths(&names, args[1]) {
                for m in &methods {
                    routes.push((m.clone(), path.clone()));
                }
            }
        }
        _ => {}
    }
    let mut out: Vec<RouteFact> = Vec::new();
    for (method, path) in routes {
        for prefix in &prefixes {
            let fact = RouteFact {
                method: method.clone(),
                path: join(prefix, &path),
            };
            if !out.contains(&fact) {
                out.push(fact);
            }
        }
    }
    out
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
/// What a decorated method's verb decorator says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecoratorRoute {
    /// A route.
    Found(RouteFact),
    /// A verb-named decorator imported from a library that is no HTTP framework.
    Foreign,
    /// No verb decorator.
    Absent,
}

pub(crate) fn decorator_route(method: Node, source: &[u8], fx: &FrameworkCtx) -> DecoratorRoute {
    let Some(verb_decorator) = decorators_of(method).into_iter().find_map(|d| {
        let name = decorator_name(d, source)?;
        VERB_DECORATORS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, verb)| (d, *verb))
    }) else {
        return DecoratorRoute::Absent;
    };
    let built = fx.built();
    // A decorator imported from a module that is no HTTP framework is not a route.
    if let Some(local) = decorator_name(verb_decorator.0, source) {
        if let Some(binding) = built.bindings.value_binding(local) {
            if !super::scope::ModuleBindings::module_in(&binding.module, DECORATOR_LIBS) {
                return DecoratorRoute::Foreign;
            }
        }
    }
    let names = Names {
        source,
        consts: &built.consts,
        bindings: &built.bindings,
        scopes: &built.scopes,
    };
    let Some(own) = decorator_path(verb_decorator.0, &names) else {
        return DecoratorRoute::Absent;
    };
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
    DecoratorRoute::Found(RouteFact {
        method: verb_decorator.1.to_string(),
        path: join(&prefix, &own),
    })
}
