# 7. Rust for readers of this code

You do not need to learn all of Rust to read MeshMCP. This chapter explains, from scratch, the dozen
ideas the code leans on, each tied to a real place. If you know another language, the comparison is
given.

## Building and running

- **Cargo** is Rust's build tool and package manager (like npm plus a build system).
  `cargo build --workspace` compiles everything, `cargo test --workspace` runs tests,
  `cargo clippy` is the linter. Dependencies are listed in `Cargo.toml` and locked in `Cargo.lock`.
- A **crate** is a library or program. A **workspace** is several crates built together; this repo has
  four, in `crates/`. Each has a `src/lib.rs` (library) or `src/main.rs` (program).
- A **module** (`mod foo;`) is a file or folder inside a crate. `pub` makes an item visible outside
  its module; without it, it is private.

## Types that make states explicit

- **`enum`** is a type with a fixed set of alternatives, each of which may carry data. `NodeKind` lists
  every kind of node (`GrpcService`, `HttpEndpoint`, `KafkaTopic`, ...). `match` handles each case and
  the compiler refuses a `match` that forgets one. This is why adding a config key becomes a compile
  error until it is classified (`crates/mesh-core/src/reload_kind.rs`).
- **`struct`** is a record. `ContractNode` is a struct with a name, a kind, a file, lines.
- **`Option<T>`** is "a value or nothing" (`Some(x)` / `None`). Rust has no null: if a function can
  return nothing, its type says so, and the caller must handle it. `Option<&str>` is the subject of a
  call, or nothing.
- **`Result<T, E>`** is "a value or an error". The **`?` operator** returns the error early:
  `let x = read(path)?;` means "if this failed, return the error from this function now". Production
  code here never calls `.unwrap()` (which crashes on an error); workspace lints forbid it outside tests.

## Ownership: who is responsible for a value

Rust has no garbage collector. Every value has exactly one owner, freed when the owner goes out of
scope. You can lend a value out:

- **`&T`** is a shared, read-only borrow; **`&mut T`** is an exclusive, writable one. The compiler
  guarantees nobody modifies something while others read it. This is what removes data races at
  compile time.
- **Lifetimes** (`'a`, `'g`) name how long a borrow lasts. You will see them in `GrpcTrace<'g>`: the
  result holds references into the graph, so it cannot outlive the snapshot it came from. You can
  usually read `'g` as "borrowed from the graph" and move on.

## Sharing across threads

- **`Arc<T>`** is a reference-counted pointer: many owners share one value, freed when the last owner
  drops it. Like a shared pointer in C++. `FilePath = Arc<Path>` means every node from one file shares a
  single path string.
- **`ArcSwap<T>`** is a cell holding an `Arc<T>` that can be replaced atomically while readers keep
  the old one. This is the snapshot mechanism of chapter 5: `state.snapshot()` is a lock-free read,
  `install_snapshot` publishes a new one.
- **`Mutex<T>`** protects a value that threads take turns modifying. Used for writers (`reload_lock`),
  never for the graph readers use.
- **`Send`** is a marker meaning "may be moved to another thread". Bounds such as `Args: Send +
  'static` on a tool say its arguments can be handed to a worker thread.

## Traits: shared behaviour

A **trait** is an interface: a set of functions a type can implement. `McpTool`
(`crates/mesh-server/src/tools/mod.rs`) says a tool has a `NAME`, an `Args` type, and a `run`
function. Each of the six tools implements it, and `ToolRegistry` handles all of them the same way.
The `derive` attribute (`#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]`) asks the
compiler to generate trait implementations for you; `JsonSchema` is how the tool's argument schema and
`#[serde(deny_unknown_fields)]` (reject unknown arguments) come for free.

## Two kinds of concurrency

- **async / Tokio.** `async fn` functions can pause at an `.await` without holding a thread. Tokio
  runs thousands of such tasks on a few threads. The message loop and socket handling are async.
  Rule: never block inside async code. Anything that reads disk or runs tree-sitter goes through
  **`spawn_blocking`**, which moves it to a separate pool of threads.
- **Rayon.** `.par_iter()` turns a loop over files into a parallel one on a pool of worker threads.
  This is how indexing parses many files at once. The fold step afterwards is sequential, on purpose,
  so the result is deterministic.

A guard obtained from `ArcSwap` must not be held across an `.await`; the code is arranged so it never is.

## `unsafe` and the operating system

Rust's safety rules are enforced by the compiler. `unsafe { ... }` marks the places where the
programmer promises something the compiler cannot check, mostly calls into C libraries or the OS.
In this repository it is small and local (the list below is the main places, not exhaustive): operating-system calls such as setting thread priority
(`crates/mesh-core/src/rescan.rs`), resolving the home directory (`root_guard.rs`), socket and Git
directory handling (`socket.rs`, `watcher/git.rs`), and the YAML and Markdown readers
(`yaml_stream.rs` drives libyaml through `unsafe-libyaml`; `markdown.rs` also has an `unsafe` block). Everything else is
safe Rust. `grep -rn unsafe crates` lists them all.

## Tests

Tests live next to the code in `#[cfg(test)] mod tests` blocks (marked `#[test]`) and in a crate's
`tests/` folder. `cargo test --workspace` runs them all. Several tests are architectural guards, not
just feature tests:

- the JSON schemas in `docs/mcp-tools.md` must match what each tool advertises
  (`test_mcp_tools_md_schema_drift_check`);
- every config key must be classified for reload; an incremental reload must equal a full rebuild;
- the same workspace must hash to one fingerprint across runs (`scripts/determinism.sh`).

## Reading a function signature

```rust
fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError>;
```

Read it left to right: "`run` takes a read-only borrow of the arguments and a read-only borrow of the
shared state, and gives back either a `ToolOutput` or a `ToolError`." No hidden side effects, no
exceptions: the type tells you the failure is a value.

## Glossary of symbols

| Syntax | Read it as |
| :--- | :--- |
| `&x` / `&mut x` | borrow `x` read-only / writable |
| `x?` | return the error now if `x` failed |
| `Some(x)` / `None` | a value / nothing |
| `Vec<T>` | a growable list of `T` |
| `HashMap<K, V>` / `BTreeMap<K, V>` | a lookup table; the second keeps its keys in order |
| `impl Trait for Type` | `Type` implements `Trait` |
| `'a` | a lifetime name |
| `#[...]` | an attribute: a compiler instruction on the next item |
| `::` | path separator: `mesh_core::types::NodeKind` |
| `pub` | visible outside this module |

Next: [8. Reading the code](08-reading-the-code.md).
