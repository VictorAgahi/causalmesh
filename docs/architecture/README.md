# MeshMCP architecture guide

A step-by-step path into how MeshMCP works, written for someone who may not know Rust. Read the
chapters in order the first time; each one adds a layer and only uses ideas from the chapters
before it.

| # | Chapter | You will understand | Rust needed |
| :-: | :--- | :--- | :--- |
| 1 | [The big picture](01-big-picture.md) | What problem it solves, the pieces, the life of one question | none |
| 2 | [From files to facts](02-from-files-to-facts.md) | How source code becomes a list of small facts | none |
| 3 | [The graph](03-the-graph.md) | How facts from different repositories get connected, and what "confidence" means | none |
| 4 | [Answering a question](04-answering-a-question.md) | The protocol, the six tools, output limits, the audit log | a little |
| 5 | [Processes and time](05-processes-and-time.md) | The daemon, snapshots, file watching, config reload, the cache | a little |
| 6 | [Safety and limits](06-safety-and-limits.md) | What is protected, how, and what is not guaranteed | a little |
| 7 | [Rust for readers of this code](07-rust-for-readers.md) | The dozen Rust ideas this code relies on, explained from scratch | this is the lesson |
| 8 | [Reading the code](08-reading-the-code.md) | Where to open which file, in which order, with exercises | yes |

The single-file reference, with every constant, timeout and edge case, is
[`../architecture.md`](../architecture.md). The guide trades completeness for understanding; when
the two seem to differ, the reference and the code win, and the guide should be fixed.

Chapters 4 and 5 name a few Rust types (`ArcSwap`, `spawn_blocking`, a trait). Each is explained in
plain words where it appears; chapter 7 is the full primer if you want more. Chapter 8 is for
contributors: it assumes a Rust toolchain.

Companion pages:
- [`../parser-support.md`](../parser-support.md): what each language contributes.
- [`../mcp-tools.md`](../mcp-tools.md): exact tool arguments and outputs.
- [`../development.md`](../development.md): building, testing, extending.
- [`../../CHANGELOG.md`](../../CHANGELOG.md): why things are the way they are, release by release.

## Conventions used in the guide

- A term in **bold** the first time it is defined is in the glossary at the end of chapter 1.
- `path/to/file.rs` names are real files; open them after reading the chapter. Line numbers are
  not given on purpose, since they move.
- "Fact" means something extracted from a file. "Edge" means a connection between two facts that
  the graph derived. "Confidence" is how sure that connection is.
- Statements about behaviour are checked against the code at the version in the root
  `Cargo.toml`. Where behaviour depends on the operating system, the guide says which.
