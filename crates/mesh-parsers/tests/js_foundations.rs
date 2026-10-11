//! JavaScript / TypeScript module-system and declaration foundations (7.1.0):
//! CommonJS and dynamic imports, ambient declarations, namespaces and anonymous
//! default exports, through the production `PolyglotIndexer` dispatch.

use mesh_core::NodeKind;
use mesh_parsers::{AstDecapitator, AstGuard, FileIndex, LanguageKind, PolyglotIndexer};
use std::path::Path;

fn index(file: &str, code: &str) -> FileIndex {
    let fi = PolyglotIndexer::extract(Path::new(file), code, 0);
    assert!(!fi.parse_failed, "{file} must parse");
    fi
}

fn node_names(fi: &FileIndex) -> Vec<String> {
    fi.nodes.iter().map(|n| n.name.to_string()).collect()
}

fn dep_targets(fi: &FileIndex) -> Vec<String> {
    let mut deps: Vec<String> = fi.dependencies.iter().map(|(_, d)| d.to_string()).collect();
    deps.sort();
    deps.dedup();
    deps
}

#[test]
fn extensions_map_to_their_own_grammar() {
    for p in ["a.js", "a.jsx", "a.mjs", "a.cjs"] {
        assert_eq!(LanguageKind::from_path(p), LanguageKind::JavaScript, "{p}");
    }
    assert_eq!(LanguageKind::from_path("a.tsx"), LanguageKind::Tsx);
    for p in ["a.ts", "a.mts", "a.cts", "a.d.ts"] {
        assert_eq!(LanguageKind::from_path(p), LanguageKind::TypeScript, "{p}");
    }
    assert!(AstGuard::verify_all_parsers());
}

#[test]
fn commonjs_require_is_a_dependency_not_a_declaration() {
    let fi = index(
        "svc/server.js",
        "const express = require('express');\n\
         const { db, pool: p } = require('./db');\n\
         const app = express();\n\
         function find(id) { return db.get(id); }\n\
         module.exports = { app };\n",
    );
    let names = node_names(&fi);
    assert!(!names.contains(&"express".to_string()), "{names:?}");
    assert!(!names.contains(&"db".to_string()), "{names:?}");
    assert!(names.contains(&"app".to_string()), "{names:?}");
    // `module.exports = { … }` declares the file's own symbol.
    assert!(names.contains(&"server".to_string()), "{names:?}");
    let deps = dep_targets(&fi);
    for expected in ["express", "./db", "db"] {
        assert!(
            deps.contains(&expected.to_string()),
            "{expected} in {deps:?}"
        );
    }
    // A destructured name is attributed only where it is used, like an ES
    // named import: `pool` is never referenced.
    assert!(!deps.contains(&"pool".to_string()), "{deps:?}");
}

#[test]
fn require_only_file_is_still_listed_through_a_module_node() {
    let fi = index(
        "svc/boot.cjs",
        "const a = require('./a');\nrequire('./side-effect');\n",
    );
    assert_eq!(fi.nodes.len(), 1);
    assert_eq!(fi.nodes[0].kind, NodeKind::Module);
    let deps = dep_targets(&fi);
    assert!(deps.contains(&"./a".to_string()), "{deps:?}");
    assert!(deps.contains(&"./side-effect".to_string()), "{deps:?}");
}

#[test]
fn dynamic_import_and_nested_require_are_dependencies() {
    let fi = index(
        "web/lazy.js",
        "export async function load() {\n  const m = await import('./heavy');\n  return m;\n}\n\
         export function legacy() {\n  return require('left-pad')('x', 3);\n}\n",
    );
    let deps = dep_targets(&fi);
    assert!(deps.contains(&"./heavy".to_string()), "{deps:?}");
    assert!(deps.contains(&"left-pad".to_string()), "{deps:?}");
}

#[test]
fn computed_specifiers_are_not_dependencies() {
    let fi = index(
        "web/dyn.js",
        "export function f(name) {\n  require(name);\n  import(`./x/${name}`);\n  require.resolve('nope');\n}\n",
    );
    assert!(dep_targets(&fi).is_empty(), "{:?}", fi.dependencies);
}

#[test]
fn import_equals_require_is_a_dependency() {
    let fi = index(
        "svc/legacy.ts",
        "import fs = require('node:fs');\nexport class A {}\n",
    );
    assert!(dep_targets(&fi).contains(&"node:fs".to_string()));
}

#[test]
fn type_only_imports_and_reexports_remain_dependencies() {
    let fi = index(
        "svc/t.ts",
        "import type { A } from './a';\nexport * as ns from './b';\nexport class C {}\n",
    );
    let deps = dep_targets(&fi);
    assert!(deps.contains(&"./a".to_string()), "{deps:?}");
    assert!(deps.contains(&"./b".to_string()), "{deps:?}");
}

#[test]
fn ambient_declarations_and_namespaces_are_indexed() {
    let fi = index(
        "types/api.d.ts",
        "export declare function f(a: number): string;\n\
         export declare function f(a: string): string;\n\
         declare const VERSION: string;\n\
         export declare namespace Api { class Client {} }\n\
         declare module 'express-serve-static-core' { interface Request { id: string } }\n\
         declare class K {}\n",
    );
    let names = node_names(&fi);
    assert_eq!(
        names.iter().filter(|n| *n == "f").count(),
        1,
        "overloads: {names:?}"
    );
    for expected in ["VERSION", "Api", "K", "Client"] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} in {names:?}"
        );
    }
    // A string-named module augments a package; it declares no symbol.
    assert!(!names.iter().any(|n| n.contains("express")), "{names:?}");
}

#[test]
fn plain_overload_signatures_do_not_duplicate_the_implementation() {
    let fi = index(
        "svc/o.ts",
        "function o(a: string): void;\nfunction o(a: number): void;\nfunction o(a: any) {}\n",
    );
    assert_eq!(node_names(&fi), vec!["o".to_string()]);
}

#[test]
fn anonymous_default_exports_are_named_after_their_file() {
    let obj = index(
        "svc/config.ts",
        "export default { port: 3000 } satisfies object;\n",
    );
    assert_eq!(node_names(&obj), vec!["config".to_string()]);

    let component = index(
        "ui/Button.js",
        "import { defineComponent } from 'vue';\nexport default defineComponent({ name: 'B' });\n",
    );
    assert!(node_names(&component).contains(&"Button".to_string()));

    let barrel = index("routes/index.ts", "export default () => 1;\n");
    assert_eq!(node_names(&barrel), vec!["routes".to_string()]);

    let anon_class = index("svc/Svc.ts", "export default class { run() {} }\n");
    assert!(node_names(&anon_class).contains(&"Svc".to_string()));
}

#[test]
fn default_export_of_an_existing_binding_adds_no_node() {
    let fi = index("svc/app.ts", "const app = make();\nexport default app;\n");
    assert_eq!(node_names(&fi), vec!["app".to_string()]);
}

#[test]
fn commonjs_member_exports_declare_their_property() {
    let fi = index(
        "svc/util.js",
        "function helper() {}\n\
         exports.foo = function () {};\n\
         module.exports.bar = () => 1;\n\
         exports.helper = helper;\n\
         other.baz = function () {};\n",
    );
    let names = node_names(&fi);
    assert!(names.contains(&"foo".to_string()), "{names:?}");
    assert!(names.contains(&"bar".to_string()), "{names:?}");
    // An alias of an existing declaration, and an unrelated object, are not exports.
    assert_eq!(
        names.iter().filter(|n| *n == "helper").count(),
        1,
        "{names:?}"
    );
    assert!(!names.contains(&"baz".to_string()), "{names:?}");
}

#[test]
fn jsx_in_a_js_file_parses_with_the_javascript_grammar() {
    let fi = index(
        "ui/App.jsx",
        "import React from 'react';\nexport const App = () => <div className=\"a\">hi</div>;\n",
    );
    assert!(node_names(&fi).contains(&"App".to_string()));
    assert!(dep_targets(&fi).contains(&"react".to_string()));
}

#[test]
fn function_expressions_are_decapitated() {
    let code = "module.exports = function handler(req) { return doWork(req); };\n";
    let out = AstDecapitator::decapitate_auto(code, LanguageKind::JavaScript, false);
    assert!(out.contains("/* stripped */"), "{out}");
    assert!(!out.contains("doWork"), "{out}");
}

// ---- review round 1 (PR #80) ----------------------------------------------

fn rpc_targets(fi: &FileIndex) -> Vec<String> {
    let mut v: Vec<String> = fi.rpc_calls.iter().map(|(_, t)| t.to_string()).collect();
    v.sort();
    v
}

#[test]
fn module_keyword_blocks_are_indexed_but_string_modules_are_not() {
    let fi = index(
        "types/m.d.ts",
        "module M { export const x = 1; }\n\
         declare module Foo.Bar { function f(): void; }\n\
         declare module 'pkg' { export function hidden(): void; }\n\
         namespace N {}\n",
    );
    let names = node_names(&fi);
    for expected in ["M", "x", "Foo.Bar", "f", "N"] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} in {names:?}"
        );
    }
    assert!(!names.contains(&"hidden".to_string()), "{names:?}");
    assert!(!names.contains(&"pkg".to_string()), "{names:?}");
}

#[test]
fn namespace_members_are_top_level_declarations() {
    let fi = index(
        "svc/ns.ts",
        "namespace A { const x = 1; function helper() {} export class B {} }\n\
         function outer() { const local = 1; }\n",
    );
    let names = node_names(&fi);
    for expected in ["A", "x", "helper", "B", "outer"] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} in {names:?}"
        );
    }
    assert!(!names.contains(&"local".to_string()), "{names:?}");
}

#[test]
fn inherited_grpc_client_fields_resolve_in_javascript() {
    let src = "class Child extends Base {\n  run() { return this.users.GetUser({}); }\n}\n";
    let js = index("svc/child.js", src);
    let tsx = index("svc/child.tsx", src);
    assert_eq!(rpc_targets(&js), rpc_targets(&tsx), "JS must match TSX");
    assert!(
        rpc_targets(&js)
            .iter()
            .any(|t| t.starts_with("^Base#users.GetUser")),
        "{:?}",
        rpc_targets(&js)
    );
}

#[test]
fn javascript_own_client_field_is_not_mistaken_for_an_inherited_one() {
    let fi = index(
        "svc/own.js",
        "class Child extends Base {\n  users = makeClient();\n  run() { return this.users.GetUser({}); }\n}\n",
    );
    assert!(
        !rpc_targets(&fi).iter().any(|t| t.starts_with("^Base#")),
        "{:?}",
        rpc_targets(&fi)
    );
}

#[test]
fn annotated_javascript_falls_back_to_the_tsx_grammar() {
    let code = "// @flow\ntype A = { a: number };\nexport type B = string;\n\
                interface I { x: number }\n\
                export class C<T> implements I { private x: number = 1; run(): string { return ''; } }\n";
    let fi = index("rn/flow.js", code);
    let names = node_names(&fi);
    for expected in ["A", "B", "I", "C", "run"] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} in {names:?}"
        );
    }
    let out = AstDecapitator::decapitate_auto(
        "export class C<T> { run(x: number): string { return secret(x); } }\n",
        LanguageKind::JavaScript,
        false,
    );
    assert!(
        out.contains("/* stripped */") && !out.contains("secret"),
        "{out}"
    );
}

#[test]
fn a_shadowed_require_is_not_a_dependency() {
    let fi = index(
        "web/shadow.js",
        "export function f(require) { return require('x'); }\n\
         export const g = (require) => require('y');\n\
         export function h() { return require('real'); }\n",
    );
    let deps = dep_targets(&fi);
    assert!(!deps.contains(&"x".to_string()), "{deps:?}");
    assert!(!deps.contains(&"y".to_string()), "{deps:?}");
    assert!(deps.contains(&"real".to_string()), "{deps:?}");
}

#[test]
fn create_require_binding_still_loads_modules() {
    let fi = index(
        "web/esm.mjs",
        "import { createRequire } from 'node:module';\nconst require = createRequire(import.meta.url);\n\
         export function load() { return require('legacy'); }\n",
    );
    assert!(dep_targets(&fi).contains(&"legacy".to_string()));
}

#[test]
fn repeated_and_chained_commonjs_exports_are_single_nodes() {
    let fi = index(
        "svc/y.js",
        "module.exports = {};\n\
         module.exports = function () {};\n\
         exports.a = function () {};\n\
         exports.a = function () {};\n\
         exports.default = class {};\n\
         module.exports.default = () => 1;\n\
         exports = module.exports = function chained() {};\n\
         exports.p = module.exports.q = () => 2;\n",
    );
    let names = node_names(&fi);
    assert_eq!(names.iter().filter(|n| *n == "y").count(), 1, "{names:?}");
    assert_eq!(names.iter().filter(|n| *n == "a").count(), 1, "{names:?}");
    assert!(!names.contains(&"default".to_string()), "{names:?}");
    for expected in ["p", "q"] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} in {names:?}"
        );
    }
}

#[test]
fn destructured_require_with_default_value_is_a_named_import() {
    let fi = index(
        "svc/d.js",
        "const { y = 1 } = require('e');\nexport function use() { return y; }\n",
    );
    let deps = dep_targets(&fi);
    assert!(
        deps.contains(&"e".to_string()) && deps.contains(&"y".to_string()),
        "{deps:?}"
    );
}

#[test]
fn ambient_overloads_are_one_symbol_per_scope() {
    let mut code: String = (0..1000)
        .map(|i| {
            format!("declare function g{i}(): void;\ndeclare function g{i}(a: number): void;\n")
        })
        .collect();
    code.push_str("declare namespace A { function f(): void; function f(a: number): void }\n");
    code.push_str("declare namespace B { function f(): void }\n");
    code.push_str("declare function f(): void;\n");
    let fi = index("types/big.d.ts", &code);
    let names = node_names(&fi);
    for i in 0..1000 {
        assert_eq!(
            names.iter().filter(|n| **n == format!("g{i}")).count(),
            1,
            "g{i}"
        );
    }
    // `f` is declared in A, in B and at the top level: three scopes, three symbols.
    assert_eq!(names.iter().filter(|n| *n == "f").count(), 3, "{names:?}");
}

#[test]
fn require_shadowed_by_any_parameter_pattern_is_not_a_dependency() {
    let fi = index(
        "web/shadow2.js",
        "export function a(p, { require }) { return require('s1'); }\n\
         export function b(p, [require]) { return require('s2'); }\n\
         export function c(require = 1) { return require('s3'); }\n\
         export function d(...require) { return require('s4'); }\n\
         export function e({ x: require }) { return require('s5'); }\n\
         export function g(cb = (require) => 1) { return require('real'); }\n",
    );
    let deps = dep_targets(&fi);
    for shadowed in ["s1", "s2", "s3", "s4", "s5"] {
        assert!(
            !deps.contains(&shadowed.to_string()),
            "{shadowed} in {deps:?}"
        );
    }
    assert!(deps.contains(&"real".to_string()), "{deps:?}");
}

#[test]
fn decapitation_strips_generators_static_blocks_and_keeps_jsx_files_intact() {
    let code = "export function* gen() { yield compute(); }\n\
                export class K { static { initialize(); } m() { return work(); } }\n\
                export const V = () => <div>{render()}</div>;\n";
    let out = AstDecapitator::decapitate_auto(code, LanguageKind::JavaScript, false);
    for secret in ["compute", "initialize", "work(", "render"] {
        assert!(!out.contains(secret), "{secret} survived: {out}");
    }
    assert!(
        out.contains("gen") && out.contains("class K") && out.contains("V"),
        "{out}"
    );
}
