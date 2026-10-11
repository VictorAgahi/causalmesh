//! Single-file components: the `<script>` blocks of `.vue` / `.svelte` files and the
//! frontmatter + scripts of `.astro` files, with the line numbers of the original file.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use mesh_core::{ContractGraph, EdgeKind, NodeKind};
use mesh_parsers::{FileIndex, PolyglotIndexer};
use std::path::Path;

fn index(file: &str, code: &str) -> FileIndex {
    let fi = PolyglotIndexer::extract(Path::new(file), code, 0);
    assert!(!fi.parse_failed, "{file} must parse");
    fi
}

fn names(fi: &FileIndex) -> Vec<String> {
    let mut v: Vec<String> = fi.nodes.iter().map(|n| n.name.to_string()).collect();
    v.sort();
    v
}

fn deps(fi: &FileIndex) -> Vec<String> {
    let mut v: Vec<String> = fi.dependencies.iter().map(|(_, d)| d.to_string()).collect();
    v.sort();
    v.dedup();
    v
}

#[test]
fn vue_script_setup_is_read_and_the_component_is_declared() {
    let fi = index(
        "ui/src/Dashboard.vue",
        "<template>\n  <Card />\n</template>\n\n<script setup lang=\"ts\">\n\
         import { ref } from 'vue';\nimport Card from './Card.vue';\n\
         const count = ref<number>(0);\nfunction bump(n: number): void { count.value += n; }\n\
         </script>\n\n<style scoped>\n.a { color: red; }\n</style>\n",
    );
    let n = names(&fi);
    for expected in ["Dashboard", "bump", "count"] {
        assert!(n.contains(&expected.to_string()), "{expected} in {n:?}");
    }
    // Lines are the original file's: `bump` is on line 9.
    let bump = fi.nodes.iter().find(|x| x.name == "bump").expect("bump");
    assert_eq!((bump.line_start, bump.line_end), (9, 9));
    let d = deps(&fi);
    assert!(
        d.contains(&"vue".to_string()) && d.contains(&"./Card.vue".to_string()),
        "{d:?}"
    );
    // The component is a declaration spanning the file, so it owns the imports too.
    let component = fi
        .nodes
        .iter()
        .position(|x| x.name == "Dashboard")
        .expect("component");
    assert!(fi
        .dependencies
        .iter()
        .any(|(i, t)| *i == component && t == "./Card.vue"));
}

#[test]
fn vue_options_api_default_export_is_the_component() {
    let fi = index(
        "ui/Legacy.vue",
        "<template><div/></template>\n<script>\nimport api from '../api';\nexport default {\n  name: 'Legacy',\n  methods: { load() { return api.get(); } },\n};\n</script>\n",
    );
    assert_eq!(
        fi.nodes.iter().filter(|n| n.name == "Legacy").count(),
        1,
        "the default export is the component: {:?}",
        names(&fi)
    );
}

#[test]
fn two_script_blocks_and_attribute_forms() {
    let fi = index(
        "ui/Two.vue",
        "<script lang='ts'>\nexport const shared = 1;\n</script>\n<script setup lang=ts>\nimport { x } from './x';\n</script>\n<template/>\n",
    );
    let n = names(&fi);
    assert!(
        n.contains(&"shared".to_string()) && n.contains(&"Two".to_string()),
        "{n:?}"
    );
    assert!(deps(&fi).contains(&"./x".to_string()));
    // TSX components.
    let tsx = index(
        "ui/Tsx.vue",
        "<script setup lang=\"tsx\">\nexport const View = () => <div class=\"a\">x</div>;\n</script>\n",
    );
    assert!(names(&tsx).contains(&"View".to_string()));
}

#[test]
fn svelte_module_and_instance_scripts() {
    let fi = index(
        "web/src/lib/Counter.svelte",
        "<script context=\"module\" lang=\"ts\">\n  export const prerender = true;\n</script>\n\n<script lang=\"ts\">\n  import { writable } from 'svelte/store';\n  import Button from './Button.svelte';\n  export let start: number = 0;\n</script>\n\n<Button />\n",
    );
    let n = names(&fi);
    assert!(
        n.contains(&"Counter".to_string()) && n.contains(&"prerender".to_string()),
        "{n:?}"
    );
    let d = deps(&fi);
    assert!(d.contains(&"svelte/store".to_string()) && d.contains(&"./Button.svelte".to_string()));
}

#[test]
fn astro_frontmatter_and_client_scripts() {
    let fi = index(
        "site/src/components/Card.astro",
        "---\nimport Layout from '../layouts/Layout.astro';\nimport { getCollection } from 'astro:content';\nconst posts = await getCollection('blog');\n---\n<Layout>\n  <h1>Hi</h1>\n  <script>\n    import confetti from 'canvas-confetti';\n    confetti();\n  </script>\n</Layout>\n",
    );
    let d = deps(&fi);
    for expected in [
        "../layouts/Layout.astro",
        "astro:content",
        "canvas-confetti",
    ] {
        assert!(d.contains(&expected.to_string()), "{expected} in {d:?}");
    }
    assert!(names(&fi).contains(&"Card".to_string()));
}

#[test]
fn broker_clients_in_a_component_script_are_detected() {
    let fi = index(
        "ui/Live.vue",
        "<script setup lang=\"ts\">\nimport { connect } from 'nats.ws';\nasync function start() {\n  const nc = await connect({ servers: 'wss://x' });\n  nc.subscribe('prices.>');\n  nc.publish('hello', 'x');\n}\n</script>\n",
    );
    let consumed: Vec<String> = fi.consumers.iter().map(|(_, t)| t.to_string()).collect();
    let produced: Vec<String> = fi.producers.iter().map(|(_, t)| t.to_string()).collect();
    assert_eq!(consumed, vec!["prices.>".to_string()]);
    assert_eq!(produced, vec!["hello".to_string()]);
}

#[test]
fn a_component_with_no_script_is_still_a_declaration_and_a_page_by_location() {
    let card = index("ui/Card.vue", "<template><div>x</div></template>\n");
    assert_eq!(names(&card), vec!["Card".to_string()]);
    let page = index("web/src/routes/blog/[slug]/+page.svelte", "<h1>x</h1>\n");
    let routes: Vec<String> = page
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::HttpEndpoint)
        .map(|n| n.name.to_string())
        .collect();
    assert_eq!(routes, vec!["PAGE /blog/:slug".to_string()]);
    let astro_page = index("site/src/pages/about.astro", "<h1>About</h1>\n");
    assert!(astro_page.nodes.iter().any(|n| n.name == "PAGE /about"));
    // A component outside `pages/` is not a page.
    let component = index("site/src/components/Nav.astro", "<nav/>\n");
    assert!(component
        .nodes
        .iter()
        .all(|n| n.kind != NodeKind::HttpEndpoint));
}

#[test]
fn an_imported_component_links_to_its_declaration_in_the_graph() {
    let mut graph = ContractGraph::new();
    PolyglotIndexer::extract(
        Path::new("ui/Button.vue"),
        "<script setup lang=\"ts\">\nconst label = 'ok';\n</script>\n",
        0,
    )
    .apply(&mut graph);
    PolyglotIndexer::extract(
        Path::new("ui/App.vue"),
        "<script setup lang=\"ts\">\nimport Button from './Button.vue';\n</script>\n<template><Button /></template>\n",
        0,
    )
    .apply(&mut graph);
    graph.reconcile_edges();
    let name = |id| {
        graph
            .get_node(id)
            .map(|n| n.name.to_string())
            .unwrap_or_default()
    };
    assert!(graph
        .all_edges()
        .iter()
        .any(|e| e.kind == EdgeKind::Imports && name(e.from) == "App" && name(e.to) == "Button"));
}

#[test]
fn malformed_components_do_not_panic_or_parse_the_template() {
    for code in [
        "<script",
        "<script>",
        "<script>\nconst a = ;;\n</script>\n",
        "<scripts>not a script</scripts><script-foo>x</script-foo>",
        "<template><script>alert(1)</script></template>\n",
        "---\nunclosed frontmatter\n",
        "<script src=\"./external.ts\"></script>\n",
    ] {
        let _ = PolyglotIndexer::extract(Path::new("ui/X.vue"), code, 0);
        let _ = PolyglotIndexer::extract(Path::new("ui/X.astro"), code, 0);
        let _ = PolyglotIndexer::extract(Path::new("ui/X.svelte"), code, 0);
    }
}

// ---- review round 1 (PR #85) ------------------------------------------------------------

#[test]
fn long_template_and_style_lines_do_not_hide_the_script() {
    let svg = format!("<svg><path d=\"{}\"/></svg>", "M0 0 L10 10 ".repeat(200));
    let fi = index(
        "ui/Long.vue",
        &format!(
            "<template>\n  {svg}\n</template>\n<script setup lang=\"ts\">\nimport z from './z';\nfunction realLong(): void {{}}\n</script>\n<style>\n.bg {{ background: url(data:image/png;base64,{}) }}\n</style>\n",
            "AAAA".repeat(600)
        ),
    );
    assert!(
        names(&fi).contains(&"realLong".to_string()),
        "{:?}",
        names(&fi)
    );
    assert!(deps(&fi).contains(&"./z".to_string()));
}

#[test]
fn astro_frontmatter_survives_a_bom_blank_lines_and_script_text() {
    let bom = index(
        "site/src/components/Bom.astro",
        "\u{feff}---\nimport L from '../L.astro';\nfunction fm(): void {}\n---\n<L/>\n",
    );
    assert!(names(&bom).contains(&"fm".to_string()), "{:?}", names(&bom));
    assert!(deps(&bom).contains(&"../L.astro".to_string()));
    let blank = index(
        "site/src/components/Blank.astro",
        "\n\n---\nfunction fm2(): void {}\n---\n<p/>\n",
    );
    assert!(names(&blank).contains(&"fm2".to_string()));
    // `<script>` written inside a frontmatter string is not a second block that eats code.
    let tricky = index(
        "site/src/components/Tricky.astro",
        "---\nconst h = '<script>x</script>';\nfunction fm3() {}\nfunction fm4() {}\n---\n<L/>\n",
    );
    let n = names(&tricky);
    assert!(
        n.contains(&"fm3".to_string()) && n.contains(&"fm4".to_string()),
        "{n:?}"
    );
}

#[test]
fn script_scanner_holes() {
    // A `<script>` mentioned in a style or a comment is not an element.
    let style = index(
        "ui/A.vue",
        "<style>/* use <script> here */ .a {}</style>\n<script>\nfunction real() {}\n</script>\n",
    );
    assert!(
        names(&style).contains(&"real".to_string()),
        "{:?}",
        names(&style)
    );
    let comment = index(
        "ui/B.vue",
        "<!-- <script>function inComment() {}</script> -->\n<script>\nfunction real2() {}\n</script>\n",
    );
    assert!(!names(&comment).contains(&"inComment".to_string()));
    assert!(names(&comment).contains(&"real2".to_string()));
    // A self-closing external script has no body and does not swallow the next block.
    let external = index(
        "ui/C.vue",
        "<script src=\"./a.ts\" />\n<script setup lang=\"ts\">\nfunction real3(): void {}\n</script>\n",
    );
    assert!(
        names(&external).contains(&"real3".to_string()),
        "{:?}",
        names(&external)
    );
    // A `>` inside an attribute value does not end the tag, and `lang` after it still counts.
    let generic = index(
        "ui/D.vue",
        "<script setup generic=\"T extends Array<string>\" lang=\"ts\">\nimport { x } from './x';\nconst a: number = 1;\nfunction real4(v: T): void {}\n</script>\n",
    );
    assert!(
        names(&generic).contains(&"real4".to_string()),
        "{:?}",
        names(&generic)
    );
    assert!(deps(&generic).contains(&"./x".to_string()));
}

#[test]
fn non_javascript_script_blocks_are_not_read_as_javascript() {
    let fi = index(
        "ui/E.vue",
        "<script type=\"application/ld+json\">{ \"@context\": \"https://schema.org\" }</script>\n<script type=\"importmap\">{ \"imports\": {} }</script>\n<script lang=\"coffee\">x = -> 1</script>\n<script>\nfunction real5() {}\n</script>\n",
    );
    assert_eq!(
        names(&fi),
        vec!["E".to_string(), "real5".to_string()],
        "only the JavaScript block is read"
    );
}

#[test]
fn route_files_are_not_named_components_and_pages_follow_the_real_conventions() {
    let page = index(
        "web/src/routes/+page.svelte",
        "<script>\nlet x = 1;\n</script>\n",
    );
    assert!(page.nodes.iter().all(|n| n.name != "+page"));
    assert!(page.nodes.iter().any(|n| n.name == "PAGE /"));
    // Layout resets, a `routes` folder that is not `src/routes`, `_`-prefixed Astro files.
    assert!(index("web/src/routes/a/+page@.svelte", "<h1/>\n")
        .nodes
        .iter()
        .any(|n| n.name == "PAGE /a"));
    assert!(index("web/src/lib/routes/+page.svelte", "<h1/>\n")
        .nodes
        .iter()
        .all(|n| n.kind != NodeKind::HttpEndpoint));
    assert!(index("site/src/pages/_components/Card.astro", "<p/>\n")
        .nodes
        .iter()
        .all(|n| n.kind != NodeKind::HttpEndpoint));
}

#[test]
fn a_component_and_a_module_of_the_same_name_are_two_modules() {
    let mut graph = ContractGraph::new();
    for (path, code) in [
        (
            "ui/Foo.vue",
            "<script setup lang=\"ts\">\nconst a = 1;\n</script>\n",
        ),
        ("ui/Foo.ts", "export default class Foo {}\n"),
        (
            "ui/A.vue",
            "<script setup lang=\"ts\">\nimport Foo from './Foo.vue';\n</script>\n",
        ),
        (
            "ui/b.ts",
            "import Foo from './Foo.js';\nexport class B {}\n",
        ),
    ] {
        PolyglotIndexer::extract(Path::new(path), code, 0).apply(&mut graph);
    }
    graph.reconcile_edges();
    let file = |id| {
        graph
            .get_node(id)
            .map(|n| n.file_path.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let name = |id| {
        graph
            .get_node(id)
            .map(|n| n.name.to_string())
            .unwrap_or_default()
    };
    let targets = |from: &str| -> Vec<String> {
        graph
            .all_edges()
            .iter()
            .filter(|e| e.kind == EdgeKind::Imports && name(e.from) == from)
            .map(|e| file(e.to))
            .collect()
    };
    assert_eq!(targets("A"), vec!["ui/Foo.vue".to_string()]);
    assert_eq!(targets("B"), vec!["ui/Foo.ts".to_string()]);
}

// ---- review round 2 (PR #85) ------------------------------------------------------------

#[test]
fn the_block_scanner_is_linear_on_hostile_components() {
    // 380 KB of markup that used to make each iteration rescan the whole remainder.
    for unit in [
        "<scripts>\n",
        "<style></style>\n",
        "<script src=x />\n",
        "<script>1</script>\n",
        "<a \"\n",
        "<!-- x -->\n",
    ] {
        let code = unit.repeat(380_000 / unit.len());
        let start = std::time::Instant::now();
        let _ = PolyglotIndexer::extract(Path::new("ui/Big.vue"), &code, 0);
        assert!(
            start.elapsed().as_secs() < 5,
            "{unit:?} took {:?}",
            start.elapsed()
        );
    }
}

#[test]
fn script_and_style_text_inside_attributes_and_strings_is_markup_not_blocks() {
    let attr = index(
        "ui/Attr.vue",
        "<template>\n  <div :title=\"'<script>'\" :x=\"'<style>'\" :y=\"'<!--'\">x</div>\n</template>\n<script setup lang=\"ts\">\nimport { real } from './real';\nfunction attrFn(): void {}\n</script>\n<style>.a {}</style>\n",
    );
    assert!(
        names(&attr).contains(&"attrFn".to_string()),
        "{:?}",
        names(&attr)
    );
    assert!(deps(&attr).contains(&"./real".to_string()));
}

#[test]
fn attribute_names_and_values_are_case_insensitive() {
    let fi = index(
        "ui/Upper.vue",
        "<SCRIPT LANG=\"TS\">\nconst a: number = 1;\nexport function upperFn(): void {}\n</SCRIPT>\n",
    );
    assert!(names(&fi).contains(&"upperFn".to_string()));
}

#[test]
fn page_routes_use_the_first_router_pair() {
    let svelte = index("web/src/routes/api/routes/+page.svelte", "<h1/>\n");
    assert!(svelte.nodes.iter().any(|n| n.name == "PAGE /api/routes"));
    let astro = index("site/src/pages/docs/pages/x.astro", "<p/>\n");
    assert!(astro.nodes.iter().any(|n| n.name == "PAGE /docs/pages/x"));
}
