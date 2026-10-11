//! File-system routing: the location of a module, confirmed by the framework's own signals,
//! is its route (Next.js, Remix / React Router, SvelteKit, Nuxt / Nitro, Astro).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use mesh_core::NodeKind;
use mesh_parsers::{FileIndex, PolyglotIndexer};
use std::path::Path;

fn index(file: &str, code: &str) -> FileIndex {
    let fi = PolyglotIndexer::extract(Path::new(file), code, 0);
    assert!(!fi.parse_failed, "{file} must parse");
    fi
}

fn routes(fi: &FileIndex) -> Vec<String> {
    let mut out: Vec<String> = fi
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::HttpEndpoint)
        .map(|n| n.signature.as_deref().unwrap_or("").to_string())
        .collect();
    out.sort();
    out
}

fn r(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn next_app_router_route_handlers_and_pages() {
    let route = index(
        "web/src/app/api/users/[id]/route.ts",
        "export async function GET(req: Request) { return Response.json({}); }\n\
         export const POST = async () => new Response('x');\n\
         const helper = 1;\n\
         export { helper as DELETE };\n",
    );
    assert_eq!(
        routes(&route),
        r(&[
            "DELETE /api/users/:id",
            "GET /api/users/:id",
            "POST /api/users/:id"
        ])
    );
    let grouped = index(
        "web/app/(shop)/products/[...slug]/route.js",
        "export function GET() {}\n",
    );
    assert_eq!(routes(&grouped), r(&["GET /products/*slug"]));
    let page = index(
        "web/app/blog/[[...path]]/page.tsx",
        "import Link from 'next/link';\nexport default function Page() { return null; }\n",
    );
    assert_eq!(routes(&page), r(&["PAGE /blog/*path?"]));
    // A `page.tsx` of an `app/` folder that is not a Next project is not a route.
    let not_next = index(
        "ui/app/page.tsx",
        "export default function Page() { return null; }\n",
    );
    assert!(routes(&not_next).is_empty());
}

#[test]
fn next_pages_router_api_and_pages_need_a_next_import() {
    let api = index(
        "web/pages/api/orders/[orderId].ts",
        "import type { NextApiRequest, NextApiResponse } from 'next';\n\
         export default function handler(req: NextApiRequest, res: NextApiResponse) {}\n",
    );
    assert_eq!(routes(&api), r(&["ALL /api/orders/:orderId"]));
    let page = index(
        "web/pages/index.tsx",
        "import Head from 'next/head';\nexport default function Home() { return null; }\n",
    );
    assert_eq!(routes(&page), r(&["PAGE /"]));
    let skipped = index(
        "web/pages/_app.tsx",
        "import type { AppProps } from 'next/app';\nexport default function App() { return null; }\n",
    );
    assert!(routes(&skipped).is_empty());
    // Create-React-App style `src/pages`: no `next`, no routes.
    let cra = index(
        "ui/src/pages/Home.tsx",
        "import React from 'react';\nexport default function Home() { return null; }\n",
    );
    assert!(routes(&cra).is_empty());
}

#[test]
fn remix_and_react_router_flat_routes() {
    let user = index(
        "web/app/routes/users.$id.tsx",
        "import { json } from '@remix-run/node';\n\
         export async function loader() { return json({}); }\n\
         export async function action() { return null; }\n\
         export default function User() { return null; }\n",
    );
    assert_eq!(
        routes(&user),
        r(&["GET /users/:id", "PAGE /users/:id", "POST /users/:id"])
    );
    let index_route = index(
        "web/app/routes/_index.tsx",
        "import { Link } from 'react-router';\nexport default function Index() { return null; }\n",
    );
    assert_eq!(routes(&index_route), r(&["PAGE /"]));
    let folder = index(
        "web/app/routes/blog.$slug/route.tsx",
        "import { json } from '@remix-run/node';\nexport const loader = () => json({});\n",
    );
    assert_eq!(routes(&folder), r(&["GET /blog/:slug"]));
    let pathless = index(
        "web/app/routes/_auth.login.tsx",
        "import { json } from '@remix-run/node';\nexport const action = () => json({});\n",
    );
    assert_eq!(routes(&pathless), r(&["POST /login"]));
    // Components kept next to routes are not routes.
    let nested = index(
        "web/app/routes/users/Card.tsx",
        "import { Link } from 'react-router';\nexport default function Card() { return null; }\n",
    );
    assert!(routes(&nested).is_empty());
}

#[test]
fn sveltekit_server_endpoints_and_page_modules() {
    let server = index(
        "web/src/routes/api/items/[id=integer]/+server.ts",
        "import { json } from '@sveltejs/kit';\n\
         export const GET = () => json({});\nexport async function DELETE() {}\n",
    );
    assert_eq!(
        routes(&server),
        r(&["DELETE /api/items/:id", "GET /api/items/:id"])
    );
    // The page itself is `+page.svelte`; its server module only adds the form actions.
    let page = index(
        "web/src/routes/(app)/dashboard/+page.server.ts",
        "export const load = () => ({});\nexport const actions = { default: async () => {} };\n",
    );
    assert_eq!(routes(&page), r(&["POST /dashboard"]));
    let load_only = index(
        "web/src/routes/x/+page.ts",
        "export const load = () => ({});\n",
    );
    assert!(routes(&load_only).is_empty());
    let layout = index(
        "web/src/routes/+layout.ts",
        "export const prerender = true;\n",
    );
    assert!(routes(&layout).is_empty());
}

#[test]
fn nuxt_nitro_server_routes() {
    let api = index(
        "web/server/api/users/[id].get.ts",
        "export default defineEventHandler((event) => ({}));\n",
    );
    assert_eq!(routes(&api), r(&["GET /api/users/:id"]));
    let any = index(
        "web/server/api/health.ts",
        "export default eventHandler(() => 'ok');\n",
    );
    assert_eq!(routes(&any), r(&["ALL /api/health"]));
    let plain = index(
        "web/server/routes/sitemap.xml.ts",
        "export default defineEventHandler(() => '');\n",
    );
    assert_eq!(routes(&plain), r(&["ALL /sitemap.xml"]));
    let not_nitro = index(
        "svc/server/api/util.ts",
        "export default function util() {}\n",
    );
    assert!(routes(&not_nitro).is_empty());
}

#[test]
fn astro_endpoints() {
    let fi = index(
        "site/src/pages/api/posts/[slug].ts",
        "import type { APIRoute } from 'astro';\n\
         export const GET: APIRoute = ({ params }) => new Response('x');\n\
         export const POST: APIRoute = () => new Response('y');\n",
    );
    assert_eq!(
        routes(&fi),
        r(&["GET /api/posts/:slug", "POST /api/posts/:slug"])
    );
    // Verb exports without Astro are not Astro endpoints.
    let none = index("site/src/pages/util.ts", "export function GET() {}\n");
    assert!(routes(&none).is_empty());
}

#[test]
fn a_route_is_anchored_to_the_export_that_declares_it() {
    let fi = index(
        "web/app/api/x/route.ts",
        "const a = 1;\n\nexport function GET() {\n  return 1;\n}\n\nexport function POST() {}\n",
    );
    let get = fi
        .nodes
        .iter()
        .find(|n| n.name == "GET /api/x")
        .expect("GET");
    assert_eq!((get.line_start, get.line_end), (3, 5));
    let post = fi
        .nodes
        .iter()
        .find(|n| n.name == "POST /api/x")
        .expect("POST");
    assert_eq!((post.line_start, post.line_end), (7, 7));
}

#[test]
fn ordinary_modules_in_look_alike_folders_are_not_routes() {
    for (path, code) in [
        ("lib/app/util.ts", "export function GET() {}\n"),
        ("src/routes/helpers.ts", "export const GET = 1;\n"),
        ("pkg/api/client.ts", "export default class Client {}\n"),
    ] {
        assert!(routes(&index(path, code)).is_empty(), "{path}");
    }
}

#[test]
fn authjs_style_destructured_exports_and_reexports_are_routes() {
    let destructured = index(
        "web/app/api/auth/[...nextauth]/route.ts",
        "import { handlers } from '@/auth';\nexport const { GET, POST } = handlers;\n",
    );
    assert_eq!(
        routes(&destructured),
        r(&["GET /api/auth/*nextauth", "POST /api/auth/*nextauth"])
    );
    let reexport = index(
        "web/app/api/x/route.ts",
        "export { GET, POST } from './handlers';\n",
    );
    assert_eq!(routes(&reexport), r(&["GET /api/x", "POST /api/x"]));
}

#[test]
fn remix_pathless_layouts_and_escapes() {
    let remix = "import { json } from '@remix-run/node';\nexport const loader = () => json({});\nexport default function C() { return null; }\n";
    for layout in ["web/app/routes/_auth.tsx", "web/app/routes/_layout.tsx"] {
        assert!(routes(&index(layout, remix)).is_empty(), "{layout}");
    }
    assert_eq!(
        routes(&index("web/app/routes/sitemap[.]xml.tsx", remix)),
        r(&["GET /sitemap.xml", "PAGE /sitemap.xml"])
    );
    assert_eq!(
        routes(&index("web/app/routes/($lang).about.tsx", remix)),
        r(&["GET /:lang?/about", "PAGE /:lang?/about"])
    );
}

#[test]
fn next_intercepting_and_private_folders() {
    let page = "import Link from 'next/link';\nexport default function P() { return null; }\n";
    assert_eq!(
        routes(&index("web/app/(shop)/@modal/(.)photo/[id]/page.tsx", page)),
        r(&["PAGE /photo/:id"])
    );
    assert!(routes(&index("web/app/_components/page.tsx", page)).is_empty());
    assert!(routes(&index("web/app/dashboard/_private/page.tsx", page)).is_empty());
}

#[test]
fn nitro_handlers_are_recognised_by_what_the_default_export_calls() {
    let commented = index(
        "web/server/api/x.ts",
        "// TODO use defineEventHandler later\nconst note = 'eventHandler';\nexport default function h() {}\n",
    );
    assert!(routes(&commented).is_empty());
    let real = index(
        "web/server/api/y.ts",
        "export default defineEventHandler(() => 'ok');\n",
    );
    assert_eq!(routes(&real), r(&["ALL /api/y"]));
}

#[test]
fn directories_above_the_workspace_root_are_not_framework_folders() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // A checkout under a directory named `app` (a container's /app): its `src/server` is no router.
    let root = dunce::canonicalize(tmp.path()).expect("canon").join("app");
    std::fs::create_dir_all(root.join("src/server")).expect("mkdir");
    let file = root.join("src/server/route.ts");
    let code = "export function GET() {}\n";
    std::fs::write(&file, code).expect("write");
    let cfg = mesh_parsers::ExtractConfig {
        ts_aliases: mesh_parsers::TsAliasIndex::new(std::slice::from_ref(&root)),
        ..mesh_parsers::ExtractConfig::default()
    };
    let with_roots = PolyglotIndexer::extract_with_config(&file, code, 0, &cfg);
    assert!(routes(&with_roots).is_empty(), "{:?}", routes(&with_roots));
    // The same file where the checkout directory is the app router really is one.
    let router = tmp.path().join("web");
    std::fs::create_dir_all(router.join("app/api/x")).expect("mkdir");
    let file = dunce::canonicalize(&router)
        .expect("canon")
        .join("app/api/x/route.ts");
    std::fs::write(&file, code).expect("write");
    let cfg = mesh_parsers::ExtractConfig {
        ts_aliases: mesh_parsers::TsAliasIndex::new(
            &[dunce::canonicalize(&router).expect("canon")],
        ),
        ..mesh_parsers::ExtractConfig::default()
    };
    let ok = PolyglotIndexer::extract_with_config(&file, code, 0, &cfg);
    assert_eq!(routes(&ok), r(&["GET /api/x"]));
}
