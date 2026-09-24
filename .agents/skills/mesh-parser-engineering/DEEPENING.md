# DEEPENING: Tree-sitter Lifetimes, the Depth Scanner & Bounded Queries

Deep reference for the `mesh-parser-engineering` skill. Read the skill first.

---

## 1. The streaming-iterator borrow problem

In `tree-sitter` 0.24+, `QueryMatches` implements `streaming_iterator::StreamingIterator`
rather than `Iterator`. `StreamingIterator::next()` returns `Option<&Self::Item>`, and the
item borrows from the cursor, so matches cannot be collected while the cursor advances:

```rust
// Does not compile: the match borrows from `matches`, which `next()` mutates.
while let Some(m) = matches.next() {
    results.push(m);
}
```

The workaround in [`guard.rs`](../../../crates/mesh-parsers/src/guard.rs):

```rust
pub struct BoundedMatch<'tree> {
    pub pattern_index: usize,
    pub captures: Vec<tree_sitter::QueryCapture<'tree>>,
}
```

`execute_bounded_query` copies the captures out of each match with `m.captures.to_vec()`.
A `QueryCapture<'tree>` is a `Node<'tree>` plus a `u32` index, and `Node<'tree>` borrows
from the `Tree`, not from the cursor — so the collected `BoundedMatch` values outlive the
iterator while staying tied to the tree that must remain alive.

Practical consequence: the `Tree` must outlive every `BoundedMatch` you hold. Keep the
tree in a local binding for the whole extraction; do not return `BoundedMatch` values from
a function that owns the tree.

---

## 2. `execute_bounded_query`: two independent limits

```rust
cursor.set_match_limit(Self::QUERY_MATCH_LIMIT);
let mut matches = Vec::new();
let mut step_count = 0usize;

let mut matches_iter = cursor.matches(query, node, source);
while let Some(m) = matches_iter.next() {
    matches.push(BoundedMatch {
        pattern_index: m.pattern_index,
        captures: m.captures.to_vec(),
    });
    step_count += 1;
    if step_count >= Self::MAX_QUERY_STEPS {
        tracing::warn!(target: "mesh::parser", "Query execution step limit reached (ReDoS guard triggered)");
        break;
    }
}
```

`QUERY_MATCH_LIMIT` (500) is tree-sitter's own cap on *in-progress* matches the cursor
tracks, which bounds memory during matching. `MAX_QUERY_STEPS` (10 000) is MeshMCP's own
cap on *yielded* matches, which bounds the caller's work and time. They are not the same
number and not the same thing; queries with recursive wildcards such as
`(class_declaration (_)*)` are why both exist.

Hitting either limit is silent truncation from the extractor's point of view — the result
is simply shorter. Check the `mesh::parser` warnings on stderr when an extractor mysteriously
misses symbols in a large file.

---

## 3. The lexical depth pre-scan

Tree-sitter's C parser allocates stack proportional to grammar recursion depth; a file
with a thousand nested parentheses can overflow the native stack and take the whole
process down with `SIGSEGV` — a crash Rust cannot catch. `AstGuard::max_nesting_depth`
runs a single-pass byte scan before the C runtime is ever touched.

It is context-aware, which the naive version was not. It skips, in order: `//` comments to
end of line; `/* ... */` comments; `#` comments (Python, YAML, shell); and `"`, `'` and
backtick literals, honouring `\` escapes inside each. Only brackets surviving all of that
count:

```rust
match b {
    b'{' | b'(' | b'[' => {
        depth += 1;
        if depth > max_depth {
            max_depth = depth;
        }
    }
    b'}' | b')' | b']' => {
        depth = depth.saturating_sub(1);
    }
    _ => {}
}
```

Two details worth preserving: it returns the **maximum** depth reached rather than
short-circuiting at the limit (the caller compares against `MAX_NESTING_DEPTH`), and the
closing-bracket arm uses `saturating_sub`, so an unbalanced file cannot underflow. The
test `test_nesting_depth_ignores_strings_and_comments` pins the literal handling — a
regex-y "improvement" that re-counts brackets inside strings will fail it.

---

## 4. Why the parser is reset, not rebuilt

```rust
let out = f(parser);
// A timed-out parse leaves the parser mid-state; reset so the next file starts clean.
parser.reset();
Some(out)
```

`set_timeout_micros(PARSER_TIMEOUT_MICROS)` makes `parser.parse()` return `None` when it
runs out of budget, but the parser keeps its partial state and, without `reset()`, the
*next* file parsed on that thread resumes from it. The timeout is set once at construction
in `create_bounded_parser`, so every parser handed out by `with_parser` is bounded for its
whole life; only the state needs clearing.

The slot array is indexed by `tree_sitter_slot()` and sized by `TREE_SITTER_COUNT`:

```rust
static PARSERS: RefCell<[Option<Parser>; LanguageKind::TREE_SITTER_COUNT]> =
    const { RefCell::new([None, None, None, None, None, None]) };
```

Adding a variant without extending both the slot mapping and this initialiser is a
compile error at best and a wrong-grammar parse at worst. `Protobuf`, `Yaml` and `Unknown`
return `None` from both `tree_sitter_slot()` and `language()`, which is how `with_parser`
declines them.

---

## 5. Decapitation edge cases

`decapitate` returns `BOUNDED_ERROR_STUB` whenever `parser.parse(content, None)` returns
`None` — timeout or C-FFI failure — rather than the raw file, because returning a
multi-hundred-kilobyte file into an agent's context is the failure mode the whole guard
exists to prevent.

`collect_body_replacements` matches on `(lang_kind, node.kind())` and takes the `body`
field via `child_by_field_name("body")`, returning early once a body is replaced so nested
functions inside an already-stripped body are not visited. The TypeScript arm does more
than strip: when a function has no explicit `return_type`, it inspects the returned object
literal (`extract_returned_object_keys`) and synthesises a type hint, so the decapitated
signature still tells the reader the shape of the result. If you add a language, decide
explicitly whether you want that behaviour; the plain Java/Go arms are the simpler model.

It also carries a `depth: usize` argument, capped at `AstGuard::MAX_NESTING_DEPTH` and
incremented on the one recursive call into `node.children()`. This is not redundant with
`AstGuard::should_parse_path`'s pre-parse lexical bracket count (`guard.rs`) — that count
is a rough proxy over raw bytes, and a file with long chained method calls, deep generic
nesting, or many match arms can produce a real tree-sitter CST hundreds of levels deep
while using almost no `{`/`(`/`[` characters, sailing straight past the lexical guard. That
was a confirmed, reproducible native stack overflow on `rust-lang/rust` before this depth
parameter existed — the lexical guard alone is not a substitute for bounding the walk
itself. Any new recursive AST walker added to `mesh-parsers` (a language extractor's own
`visit_node`, a `collect_*` helper) needs the same treatment, not just this one function.
