//! Routes that a file's *location* declares: the file-system routers of Next.js (App and
//! Pages), Remix / React Router, SvelteKit, Nuxt / Nitro and Astro.
//!
//! A directory name alone is not evidence (`pages/` and `routes/` exist in every UI
//! project), so each convention also needs a signal from the file itself: an import of the
//! framework, or the exports only that framework's route modules have (`GET` and `POST`
//! functions in a `route.ts`, a `+server.ts`, `loader` / `action` next to a Remix import).

use super::http::{join, RouteFact};
use super::scope::ModuleBindings;
use super::{arguments, scope::unquote};
use std::collections::HashMap;
use std::path::Path;
use tree_sitter::Node;

const VERBS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// A name the module exports and the 1-based lines of its declaration.
pub(super) type Exports = HashMap<String, (usize, usize)>;

/// Everything a module exports by name (`export function GET`, `export const POST`,
/// `export { handler as GET }`), plus `default`.
pub(super) fn exported_names(root: Node, source: &[u8]) -> Exports {
    let mut out = Exports::new();
    for stmt in root.named_children(&mut root.walk()) {
        if stmt.kind() != "export_statement" {
            continue;
        }
        let lines = (stmt.start_position().row + 1, stmt.end_position().row + 1);
        if stmt
            .children(&mut stmt.walk())
            .any(|c| c.kind() == "default")
        {
            out.insert("default".to_string(), lines);
            // `export default defineEventHandler(…)`: remember what is called.
            let callee = stmt
                .child_by_field_name("value")
                .filter(|v| v.kind() == "call_expression")
                .and_then(|v| v.child_by_field_name("function"))
                .and_then(|f| f.utf8_text(source).ok());
            if let Some(callee) = callee {
                out.insert(format!("default@{callee}"), lines);
            }
            continue;
        }
        if let Some(decl) = stmt.child_by_field_name("declaration") {
            match decl.kind() {
                "function_declaration" | "generator_function_declaration" | "class_declaration" => {
                    if let Some(name) = decl
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                    {
                        out.insert(name.to_string(), lines);
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    for d in decl
                        .named_children(&mut decl.walk())
                        .filter(|d| d.kind() == "variable_declarator")
                    {
                        let Some(binding) = d.child_by_field_name("name") else {
                            continue;
                        };
                        match binding.kind() {
                            "identifier" => {
                                if let Ok(name) = binding.utf8_text(source) {
                                    out.insert(name.to_string(), lines);
                                }
                            }
                            // `export const { GET, POST } = handlers;` (Auth.js)
                            "object_pattern" => {
                                for member in binding.named_children(&mut binding.walk()) {
                                    let local = match member.kind() {
                                        "shorthand_property_identifier_pattern" => Some(member),
                                        "pair_pattern" => member.child_by_field_name("value"),
                                        "object_assignment_pattern" => {
                                            member.child_by_field_name("left")
                                        }
                                        _ => None,
                                    };
                                    if let Some(name) = local.and_then(|n| n.utf8_text(source).ok())
                                    {
                                        out.insert(name.to_string(), lines);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        // `export { handler as GET, POST }`
        for clause in stmt
            .named_children(&mut stmt.walk())
            .filter(|c| c.kind() == "export_clause")
        {
            for spec in clause
                .named_children(&mut clause.walk())
                .filter(|s| s.kind() == "export_specifier")
            {
                let exported = spec
                    .child_by_field_name("alias")
                    .or_else(|| spec.child_by_field_name("name"))
                    .and_then(|n| n.utf8_text(source).ok());
                if let Some(name) = exported {
                    out.insert(name.to_string(), lines);
                }
            }
        }
    }
    out
}

/// Whether the location could be a route's at all (a cheap test made before the file is analysed).
pub(crate) fn path_may_route(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|f| f.starts_with('+'))
        || path.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some("app" | "pages" | "routes" | "api")
            )
        })
}

fn segments(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(str::to_string),
            _ => None,
        })
        .collect()
}

/// Splits `name.ext` into its stem and extension.
fn split_ext(file: &str) -> (&str, &str) {
    file.rsplit_once('.').unwrap_or((file, ""))
}

fn is_script_ext(ext: &str) -> bool {
    matches!(
        ext,
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts"
    )
}

/// A bracketed dynamic segment as a route parameter: `[id]` -> `:id`, `[...rest]` ->
/// `*rest`, `[[...rest]]` / `[[id]]` -> optional, `[id=int]` -> `:id`, a `(group)` or
/// `@slot` -> nothing.
fn dynamic_segment(seg: &str) -> Option<String> {
    // `(group)` and `@slot` are no path segments (intercepting markers are read in `route_of`).
    if (seg.starts_with('(') && seg.ends_with(')')) || seg.starts_with('@') {
        return None;
    }
    if let Some(inner) = seg.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
        let name = inner.trim_start_matches("...");
        let name = name.split('=').next().unwrap_or(name);
        return Some(if inner.starts_with("...") {
            format!("*{name}?")
        } else {
            format!(":{name}?")
        });
    }
    if let Some(inner) = seg.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let name = inner.split('=').next().unwrap_or(inner);
        return Some(match name.strip_prefix("...") {
            Some(rest) => format!("*{rest}"),
            None => format!(":{name}"),
        });
    }
    Some(seg.to_string())
}

fn route_of(dirs: &[String], leaf: Option<&str>) -> String {
    let mut segs: Vec<String> = Vec::new();
    for raw in dirs.iter().map(String::as_str).chain(leaf) {
        let mut seg = raw;
        // Intercepting routes are served at the path they intercept: `(.)x` the same level,
        // `(..)x` one route segment up, `(..)(..)x` two, `(...)x` from the root.
        if let Some(rest) = seg.strip_prefix("(...)") {
            segs.clear();
            seg = rest;
        } else if seg.starts_with("(..)") {
            while let Some(rest) = seg.strip_prefix("(..)") {
                segs.pop();
                seg = rest;
            }
        } else if let Some(rest) = seg.strip_prefix("(.)") {
            seg = rest;
        }
        if let Some(s) = dynamic_segment(seg).filter(|s| !s.is_empty()) {
            segs.push(s);
        }
    }
    if segs.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segs.join("/"))
    }
}

/// The route segments of a Remix / React Router flat-route file stem
/// (`users.$id.edit` -> `/users/:id/edit`, `_index` -> `/`, `_auth.login` -> `/login`,
/// `sitemap[.]xml` -> `/sitemap.xml`, `($lang).about` -> `/:lang?/about`).
fn remix_route(stem: &str) -> String {
    // `[.]` is an escaped dot, not a separator.
    let escaped = stem.replace("[.]", "\u{1}");
    let mut out = String::new();
    for part in escaped.split('.') {
        let part = part.trim_end_matches('_').replace('\u{1}', ".");
        if part.is_empty() || part == "_index" || part == "index" || part.starts_with('_') {
            continue;
        }
        out.push('/');
        if part == "$" {
            out.push('*');
        } else if let Some(name) = part.strip_prefix('$') {
            out.push(':');
            out.push_str(name);
        } else if let Some(inner) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
            // `($lang)` is an optional parameter, `(en)` an optional static segment.
            match inner.strip_prefix('$') {
                Some(name) => out.push_str(&format!(":{name}?")),
                None => out.push_str(&format!("{inner}?")),
            }
        } else {
            out.push_str(&part);
        }
    }
    if out.is_empty() {
        "/".to_string()
    } else {
        out
    }
}

/// The route of a single-file component page by location alone: SvelteKit's
/// `+page.svelte` and Astro's `src/pages/**/*.astro` (both unambiguous conventions).
pub(crate) fn sfc_route(path: &Path) -> Option<RouteFact> {
    let segs = segments(path);
    let (file, dirs) = segs.split_last()?;
    let (stem, ext) = split_ext(file);
    let route = match (ext, stem) {
        // `+page.svelte`, and `+page@.svelte` / `+page@(group).svelte` (layout resets).
        ("svelte", s) if s == "+page" || s.starts_with("+page@") => {
            // SvelteKit's routes live in `src/routes` (the first such pair names the root).
            let i = (1..dirs.len()).find(|&i| dirs[i] == "routes" && dirs[i - 1] == "src")?;
            route_of(&dirs[i + 1..], None)
        }
        ("astro", _) if !stem.starts_with('_') => {
            let i = (1..dirs.len()).find(|&i| dirs[i] == "pages" && dirs[i - 1] == "src")?;
            // Astro ignores files and folders whose name starts with `_`.
            if dirs[i + 1..].iter().any(|d| d.starts_with('_')) {
                return None;
            }
            route_of(&dirs[i + 1..], (stem != "index").then_some(stem))
        }
        _ => return None,
    };
    Some(RouteFact {
        method: "PAGE".to_string(),
        path: route,
    })
}

/// React Router v7 config routing: the routes `app/routes.ts` declares with `index`, `route`,
/// `layout` and `prefix` (`export default [ route('users/:id', 'routes/user.tsx') ] satisfies
/// RouteConfig`), as `PAGE` routes anchored at each call.
pub(crate) fn react_router_config(root: Node, source: &[u8]) -> Vec<(RouteFact, (usize, usize))> {
    let exported = root
        .named_children(&mut root.walk())
        .filter(|s| s.kind() == "export_statement")
        .filter(|s| s.children(&mut s.walk()).any(|c| c.kind() == "default"))
        .find_map(|s| s.child_by_field_name("value"))
        .map(crate::languages::typescript::TypeScriptExtractor::peel_wrappers);
    // `const routes = [..]; export default routes;`: follow the identifier to its array.
    let array = exported.and_then(|v| {
        if v.kind() != "identifier" {
            return Some(v);
        }
        let name = v.utf8_text(source).ok()?;
        root.named_children(&mut root.walk())
            .flat_map(|s| {
                let decl = if s.kind() == "export_statement" {
                    s.child_by_field_name("declaration")
                } else {
                    Some(s)
                };
                decl.into_iter().flat_map(|d| {
                    d.named_children(&mut d.walk())
                        .filter(|c| c.kind() == "variable_declarator")
                        .collect::<Vec<_>>()
                })
            })
            .find(|d| {
                d.child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    == Some(name)
            })
            .and_then(|d| d.child_by_field_name("value"))
            .map(crate::languages::typescript::TypeScriptExtractor::peel_wrappers)
    });
    let array = array.filter(|v| v.kind() == "array");
    let mut out = Vec::new();
    if let Some(array) = array {
        walk_route_config(array, "", source, &mut out, 0);
    }
    // An index child answers its parent's URL: one route per URL.
    let mut seen = std::collections::HashSet::new();
    out.retain(|(fact, _)| seen.insert(fact.path.clone()));
    out
}

fn walk_route_config(
    array: Node,
    prefix: &str,
    source: &[u8],
    out: &mut Vec<(RouteFact, (usize, usize))>,
    depth: usize,
) {
    if depth > 16 {
        return;
    }
    for element in array.named_children(&mut array.walk()) {
        // `...prefix('admin', [..])` spreads the call.
        let element = if element.kind() == "spread_element" {
            match element.named_child(0) {
                Some(inner) => inner,
                None => continue,
            }
        } else {
            element
        };
        if element.kind() != "call_expression" {
            continue;
        }
        let Some(name) = element
            .child_by_field_name("function")
            .and_then(|f| f.utf8_text(source).ok())
        else {
            continue;
        };
        let args = arguments(element);
        let text_of = |i: usize| args.get(i).and_then(|a| unquote(*a, source));
        let children_of = |i: usize| args.get(i).copied().filter(|a| a.kind() == "array");
        let lines = (
            element.start_position().row + 1,
            element.end_position().row + 1,
        );
        let mut page = |path: &str| {
            out.push((
                RouteFact {
                    method: "PAGE".to_string(),
                    path: path.to_string(),
                },
                lines,
            ));
        };
        match name {
            "index" => page(&join(prefix, "")),
            "route" => {
                // `route(null, file, [..])` is a pathless wrapper: its children keep the prefix.
                if args.first().is_some_and(|a| a.kind() == "null") {
                    if let Some(children) = children_of(2) {
                        walk_route_config(children, prefix, source, out, depth + 1);
                    }
                    continue;
                }
                let Some(path) = text_of(0) else { continue };
                let full = join(prefix, &path);
                page(&full);
                if let Some(children) = children_of(2) {
                    walk_route_config(children, &full, source, out, depth + 1);
                }
            }
            "layout" => {
                if let Some(children) = children_of(1).or_else(|| children_of(2)) {
                    walk_route_config(children, prefix, source, out, depth + 1);
                }
            }
            "prefix" => {
                if let (Some(path), Some(children)) = (text_of(0), children_of(1)) {
                    walk_route_config(children, &join(prefix, &path), source, out, depth + 1);
                }
            }
            _ => {}
        }
    }
}

/// The routes the location and exports of one file declare, with the lines each is anchored to.
pub(crate) fn file_routes(
    path: &Path,
    exports: &Exports,
    bindings: &ModuleBindings,
) -> Vec<(RouteFact, (usize, usize))> {
    let segs = segments(path);
    let Some((file, dirs)) = segs.split_last() else {
        return Vec::new();
    };
    let (stem, ext) = split_ext(file);
    if !is_script_ext(ext) {
        return Vec::new();
    }
    let verbs: Vec<(&str, (usize, usize))> = VERBS
        .iter()
        .filter_map(|v| exports.get(*v).map(|lines| (*v, *lines)))
        .collect();
    let whole = (1, 1);
    let page = |route: &str| {
        let lines = exports.get("default").copied().unwrap_or(whole);
        (
            RouteFact {
                method: "PAGE".to_string(),
                path: route.to_string(),
            },
            lines,
        )
    };
    let fact = |method: &str, route: &str, lines| {
        (
            RouteFact {
                method: method.to_string(),
                path: route.to_string(),
            },
            lines,
        )
    };
    // The FIRST marker names the router root: a route folder that happens to be called
    // `app` or `api` (`app/app/dashboard`, `server/api/v1/api/x`) is part of the route.
    let after = |marker: &str| -> Option<&[String]> {
        dirs.iter()
            .position(|d| d == marker)
            .map(|i| &dirs[i + 1..])
    };
    let after_pair = |parent: &str, marker: &str| -> Option<&[String]> {
        (1..dirs.len())
            .find(|&i| dirs[i] == marker && dirs[i - 1] == parent)
            .map(|i| &dirs[i + 1..])
    };

    // SvelteKit: `src/routes/**/+server.ts` (verbs) and `+page(.server).ts` (load / actions).
    if stem.starts_with('+') {
        if let Some(rest) = after_pair("src", "routes") {
            let route = route_of(rest, None);
            return match stem {
                "+server" => verbs.iter().map(|(v, l)| fact(v, &route, *l)).collect(),
                // The page itself is `+page.svelte`; its load / actions module adds the form actions.
                "+page" | "+page.server" => match exports.get("actions") {
                    Some(l) => vec![fact("POST", &route, *l)],
                    None => Vec::new(),
                },
                _ => Vec::new(),
            };
        }
    }

    // Next.js App Router: `app/**/route.ts` (verbs) and `app/**/page.tsx`.
    if (stem == "route" || stem == "page") && dirs.iter().any(|d| d == "app") {
        let rest = after("app").unwrap_or(&[]);
        // A folder prefixed with `_` is private to the app router: nothing under it is routed
        // (Remix's `app/routes/_index/route.tsx` is handled below, not here).
        if !rest.iter().any(|d| d.starts_with('_')) {
            let route = route_of(rest, None);
            if stem == "route" && !verbs.is_empty() {
                return verbs.iter().map(|(v, l)| fact(v, &route, *l)).collect();
            }
            if stem == "page"
                && exports.contains_key("default")
                && bindings.references_any(&["next"])
            {
                return vec![page(&route)];
            }
        }
    }

    // Nuxt / Nitro: `server/api/**` (under `/api`), `server/routes/**`; `x.get.ts` pins the method.
    if let Some(rest) = after_pair("server", "api") {
        if let Some(r) = nitro_route(&["api"], rest, stem, exports) {
            return r;
        }
    }
    if let Some(rest) = after_pair("server", "routes") {
        if let Some(r) = nitro_route(&[], rest, stem, exports) {
            return r;
        }
    }

    // Remix / React Router: `app/routes/users.$id.tsx` (flat routes) and the folder form
    // `app/routes/users.$id/route.tsx`.
    if let Some(rest) = after_pair("app", "routes") {
        let remix_import =
            bindings.references_any(&["@remix-run/", "react-router", "@react-router/"]);
        if remix_import {
            // A pathless layout (`_auth.tsx`, `_layout.tsx`) wraps routes, it is not one.
            let layout = stem
                .rsplit('.')
                .next()
                .is_some_and(|last| last.starts_with('_') && last != "_index");
            let route = match rest.last() {
                None if layout => None,
                None => Some(remix_route(stem)),
                // `routes/users.$id/route.tsx`: one folder, one route. Deeper nesting is not a
                // convention this reads.
                Some(folder) if rest.len() == 1 && (stem == "route" || stem == "index") => {
                    // A pathless layout in folder form (`routes/_auth/route.tsx`) is no route either.
                    let pathless = folder
                        .rsplit('.')
                        .next()
                        .is_some_and(|last| last.starts_with('_') && last != "_index");
                    (!pathless).then(|| remix_route(folder))
                }
                Some(_) => None,
            };
            if let Some(route) = route {
                let mut out = Vec::new();
                if let Some(l) = exports.get("loader") {
                    out.push(fact("GET", &route, *l));
                }
                if let Some(l) = exports.get("action") {
                    out.push(fact("POST", &route, *l));
                }
                if exports.contains_key("default") {
                    out.push(page(&route));
                }
                if !out.is_empty() {
                    return out;
                }
            }
        }
    }

    // Next.js Pages Router and Astro endpoints: both live in `pages/`.
    if let Some(rest) = after("pages") {
        let is_index = stem == "index";
        let astro_rest = after_pair("src", "pages").unwrap_or(rest);
        let leaf = (!is_index).then_some(stem);
        // Astro: `src/pages/**/x.ts` exporting verbs (`export const GET: APIRoute`).
        if !verbs.is_empty() && bindings.references_any(&["astro"]) {
            let route = route_of(astro_rest, leaf);
            return verbs.iter().map(|(v, l)| fact(v, &route, *l)).collect();
        }
        // Next: needs a `next` import, and skips `_app`, `_document`, `_error`, middleware.
        if exports.contains_key("default")
            && bindings.references_any(&["next"])
            && !stem.starts_with('_')
            && stem != "middleware"
        {
            let route = route_of(rest, leaf);
            let method = if rest.first().is_some_and(|d| d == "api") {
                "ALL"
            } else {
                "PAGE"
            };
            let lines = exports.get("default").copied().unwrap_or(whole);
            return vec![fact(method, &route, lines)];
        }
    }
    Vec::new()
}

/// A Nitro route file: `server/api/users/[id].get.ts` -> `GET /api/users/:id`.
fn nitro_route(
    prefix: &[&str],
    rest: &[String],
    stem: &str,
    exports: &Exports,
) -> Option<Vec<(RouteFact, (usize, usize))>> {
    // The file must define a handler: `export default defineEventHandler(…)`.
    let lines = exports.get("default").copied()?;
    let handler = [
        "defineEventHandler",
        "eventHandler",
        "defineNuxtRouteHandler",
        "defineCachedEventHandler",
        "defineWebSocketHandler",
        "defineLazyEventHandler",
        "cachedEventHandler",
    ]
    .iter()
    .any(|h| {
        exports
            .keys()
            .filter_map(|k| k.strip_prefix("default@"))
            // `h3.defineEventHandler(...)` is the same helper under a namespace.
            .any(|callee| callee == *h || callee.ends_with(&format!(".{h}")))
    });
    if !handler {
        return None;
    }
    let (name, method) = match stem.rsplit_once('.') {
        Some((n, m)) if VERBS.iter().any(|v| v.eq_ignore_ascii_case(m)) => {
            (n, m.to_ascii_uppercase())
        }
        _ => (stem, "ALL".to_string()),
    };
    let mut dirs: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
    dirs.extend(rest.iter().cloned());
    let leaf = (name != "index").then_some(name);
    let route = route_of(&dirs, leaf);
    Some(vec![(
        RouteFact {
            method,
            path: route,
        },
        lines,
    )])
}
