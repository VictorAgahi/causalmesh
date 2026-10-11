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
