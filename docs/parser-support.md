# Parser support: how deep each language goes

MeshMCP does not understand every language equally. TypeScript and JavaScript are read at a much
higher level than C#, and C# than Ruby. This page says, per language and file format, what ends up
in the graph and where it stops, so you can judge how far an answer can be trusted on your code.

The statements here are checked against the extractors in `crates/mesh-parsers/src/languages/`
and against `CHANGELOG.md`. They describe what the code recognises, **not** how accurate it is on
your code: the only extraction accuracy measured against hand-written ground truth is gRPC edges
on three open-source demo applications ([`quality.md`](quality.md), and the README). Everything
else below is "recognised and unit-tested", not "validated".

---

## 1. The four levels

| Level | Name | What the language contributes |
| :---: | :--- | :--- |
| 1 | Declarations | Classes, interfaces and similar symbols, found by `smart_search` and returned with bodies stripped. At most one route idiom (Rails, Laravel / Symfony, Vapor, Akka HTTP). No imports, no gRPC, no events. |
| 2 | Contracts, no imports | Level 1 plus several contract idioms read from annotations and calls (HTTP routes, Kafka). No import graph, so `find_dependents` finds nothing for these files. |
| 3 | Imports and contracts | Level 2 plus the import graph (`find_dependents`), recognised by per-language idioms. Which of gRPC, HTTP and brokers are covered varies: each section says. Written "3-" below when a whole family is missing. |
| 4 | Semantic resolution | Level 3 plus scope-aware resolution inside a file and across files: constants and enums behind a topic name, path aliases, the client an object was built from, composed route prefixes, file-system routing. Only the JavaScript / TypeScript extractor does this; the level is defined by those features, so no other language can reach it as things stand. |

A level is a description of depth, not a quality score. The levels are not strictly cumulative (C#
has Kafka and ASP.NET routes but no imports; Rust has imports but no gRPC clients), so read the
capability table of the language rather than the number. Every level is pattern-based: there is no
type checker behind any of them.

| Language / format | Level | Section |
| :--- | :---: | :--- |
| TypeScript, JavaScript (and `.vue` / `.svelte` / `.astro` scripts) | 4 | [2.1](#21-typescript-and-javascript-level-4) |
| Java | 3 | [2.2](#22-java-level-3) |
| Go | 3- (Kafka only; no gRPC beyond `Register*`/`New*Client`) | [2.3](#23-go-level-3) |
| Python | 3 | [2.4](#24-python-level-3) |
| Rust | 3- (no gRPC clients) | [2.5](#25-rust-level-3) |
| C++ | 3- (includes and gRPC servers only) | [2.6](#26-c-level-3-narrow) |
| Kotlin | 2 | [2.7](#27-kotlin-and-c-level-2) |
| C# | 2 | [2.7](#27-kotlin-and-c-level-2) |
| Ruby, PHP, Swift, Scala | 1 (plus one route idiom each) | [2.8](#28-ruby-php-swift-scala-level-1) |
| Protobuf | contract source | [3.1](#31-protobuf) |
| OpenAPI, AsyncAPI, Spring properties | contract source | [3.2](#32-openapi-asyncapi-spring-properties) |
| Markdown | docs | [3.3](#33-markdown) |

---

## 2. Source languages

Extensions read: Java `.java`; Go `.go`; Python `.py`; Rust `.rs`; Kotlin `.kt .kts`; C# `.cs`; Ruby `.rb .rake`; PHP `.php .phtml`; Swift `.swift`; Scala `.scala .sc`; C++ `.cpp .cc .cxx .hpp .hh .hxx .h` (there is no C grammar; `.h` is read as C++); Protobuf `.proto`; TypeScript / JavaScript as in 2.1.

All source languages go through the same guards before tree-sitter runs (size, binary, line
length, nesting; see [architecture](architecture.md)) and are decapitated by `smart_search` the
same way. What differs is the extractor.

### 2.1 TypeScript and JavaScript (level 4)

Grammars: TypeScript, TSX and JavaScript (own grammar; a file with Flow or Babel annotations that
does not parse as JavaScript is re-parsed as TSX). Extensions: `.ts .mts .cts .tsx .js .jsx .mjs
.cjs`. Single-file components `.vue`, `.svelte` and `.astro` are read through their `<script>`
blocks, with the original line numbers.

| Capability | What is recognised |
| :--- | :--- |
| Declarations | classes, interfaces, functions, enums, type aliases, `const`/`let`, `declare` forms, namespaces, anonymous `export default`, CommonJS `module.exports` / `exports.x` |
| Imports | `import`, `require()`, dynamic `import()`, `import x = require()`, destructured requires; `tsconfig` / `jsconfig` `paths`, `baseUrl`, `extends` chains and `package.json` `imports` aliases; a relative import links to the file it names |
| gRPC | NestJS `@GrpcMethod` (arguments resolved through enums and imported constants), `getService<...>()` clients and their method calls, imported `new XClient(...)` |
| Message brokers | a rule table gated on the libraries a file loads and on the client it built: Kafka family, Redis (pub/sub, streams, list queues), NATS, MQTT, RabbitMQ, BullMQ / Bull, SQS / SNS, Kinesis, EventBridge, Google Pub/Sub, Pulsar, Azure Service Bus and Event Hubs, STOMP, NestJS microservices |
| HTTP routes | Express, Fastify, Koa, Hono, Elysia, h3, restify, polka, itty-router, hapi (with composed prefixes and in-file mounts); decorated controllers (NestJS, routing-controllers, tsoa, inversify); file-system routers (Next.js, Remix / React Router, SvelteKit, Nuxt / Nitro, Astro) |
| Typed APIs | tRPC procedures, GraphQL operations (`@nestjs/graphql`, `type-graphql`, resolver maps) |
| Name resolution | string literals, templates, `+` concatenation, module `const`s, `as const` / `Object.freeze` objects, string enums, class members, function-local constants, scope-aware (a parameter or `let` hides a module constant) |

> **If you run NestJS, read this.** Not resolved: the application's global prefix
> (`setGlobalPrefix`) and `@Version` (set `http_prefix` for a constant prefix), inherited controller
> bases, and any topic or queue name that comes from `ConfigService`, `process.env` or a config object.
> Expect missing routes and "not statically resolved" on topics wherever production code is
> configuration-driven; the label "level 4" describes the extractor, not the recall on your code.

Not recognised at all (non-exhaustive; absence from the table above means invisible): Socket.IO /
NestJS WebSocket gateways, EventEmitter2 / `@OnEvent`, Temporal, NestJS CQRS buses and sagas, `@Cron`
schedulers, server-sent events, ORMs (Prisma, TypeORM). The BullMQ decorators `@Processor` /
`@InjectQueue`, `@nestjs/bull(mq)`, `@RabbitSubscribe` / `@RabbitRPC` and `@SqsMessageHandler` are
recognised.

Where it stops (all listed in `CHANGELOG.md` 7.1.0 to 7.1.10):
- A name taken from a parameter, `process.env`, a config object or a runtime registry is not a
  name and records nothing. Declare the registry with a custom pattern.
- NestJS global prefixes and `@Version`, Express/Koa mounts across files, a Fastify plugin passed
  as a named function, inherited controller bases, and HTTP client calls are not linked.
- Template-only component usage (auto-imported Vue/Nuxt components with no `import`) is not seen.
- A Kafka topic, a Redis channel and a queue that happen to share a name are one topic in the graph
  (the key is the lower-cased name, whatever the broker), so a consumer of one can appear linked to a
  producer of another.
- `export =`, UMD/webpack wrappers, namespace-qualified constants (`ns.Enum.MEMBER`) are not
  resolved.
- TanStack Router, Expo Router, Qwik City and SolidStart conventions are not covered.

Why this is the deepest: JavaScript and TypeScript are the most frequent languages in the
workspaces that drove the work, and they have no single dominant framework, so the extractor is a
rule table (`languages/js_frameworks/`) plus per-file scope facts rather than a handful of
hard-coded cases. That also makes it the most heuristic-heavy part of the codebase: every rule is
a convention, and a convention the table lacks is invisible.

### 2.2 Java (level 3)

| Capability | What is recognised |
| :--- | :--- |
| Declarations | classes, interfaces, methods |
| Imports | `import` statements, for `find_dependents` |
| gRPC | `*ImplBase` servers, `@GrpcService`, `newBlockingStub` / `newStub` / `newFutureStub` clients |
| Events | `@KafkaListener` (consumers), `KafkaTemplate` (producers); topic names resolved through Spring `application*.yml` / `.properties` placeholders when `[engines.contracts.spring] resolve_placeholders` is on |
| HTTP | Spring MVC mappings, JAX-RS annotations |

**You will not get:** JMS, RabbitMQ, Spring Cloud Stream, Kafka Streams or SQS; Feign / RestTemplate /
WebClient links to routes; WebFlux functional routes; a topic given as a constant or enum.

Stops: this extractor has no constant propagation (a topic built in code is not resolved; use
Spring properties), and it reads only the Kafka and HTTP idioms named above.

### 2.3 Go (level 3)

| Capability | What is recognised |
| :--- | :--- |
| Declarations | structs/types as classes, interfaces, funcs and methods |
| Imports | import paths |
| gRPC | `Register*Server` servers, `New*Client` clients (receivers `redis`, `http`, `mongo`, `s3`, `sqs`, `vault`, `elastic`, `sql`, `db`, `kafka`, `sarama` are excluded) |
| Events | Kafka via `sarama`, `kafka-go`, `confluent-kafka-go`; string constants in the file are resolved |
| HTTP | any `.Get/.Post/...("/literal", handler)` call with two or more arguments, whatever the receiver (chi, gin, echo shapes); a non-router call of that shape also matches |

Stops: no broker other than Kafka is read by the Go extractor; only the router-style route calls
above are read; constants are resolved within the file, not across packages.

### 2.4 Python (level 3)

| Capability | What is recognised |
| :--- | :--- |
| Declarations | classes, functions (docstrings kept when bodies are stripped) |
| Imports | `import` / `from ... import` |
| gRPC | `*Servicer` servers, `*_pb2_grpc.*Stub(...)` clients |
| Events | `confluent_kafka`, `aiokafka` producers and consumers; Celery tasks as queues; module constants resolved, an environment variable lookup is not a topic |
| HTTP | routes on decorators of `app`, `router` or `api_router` (Flask, FastAPI); a Blueprint (`@bp.route`) is not read |

Stops: only the frameworks and libraries named above are read; a topic name that comes from a
settings object or an environment variable is not recorded.

### 2.5 Rust (level 3)

| Capability | What is recognised |
| :--- | :--- |
| Declarations | structs/enums/traits, functions |
| Imports | `use`, `extern crate` |
| gRPC | tonic **servers** (`#[tonic::async_trait]` `impl`). Client stubs are not recognised |
| Events | `rdkafka` (`.subscribe(&["topic"])`, `FutureRecord::to("topic")`) |
| HTTP | `axum` `Router::route("/path", get(handler))`, actix-web attribute macros |

Stops: only the idioms named above are read (no gRPC clients, no other brokers or web
frameworks).

### 2.6 C++ (level 3, narrow)

Declarations (classes, structs, free functions; a class whose methods are all pure virtual is an
interface), quoted `#include "x.h"` as dependencies (angle-bracket system headers are ignored,
`[engines.contracts.cpp] include_paths` resolves search paths), and gRPC servers (classes deriving
`::grpc::Service`, including out-of-line `Foo::Method` definitions). No client stubs, no events,
no import resolution through build systems (CMake, Bazel).

### 2.7 Kotlin and C# (level 2)

| | Kotlin | C# |
| :--- | :--- | :--- |
| Declarations | classes, objects, interfaces, functions | classes, interfaces, methods |
| Imports | not extracted | not extracted |
| Events | Spring `@KafkaListener`; `kafka-clients` `.subscribe` / `.send` / `.produce`; string defaults resolved | Confluent.Kafka producers and consumers, **string literal topics only** (a variable argument records nothing rather than the variable's name) |
| HTTP | Spring annotations | ASP.NET attributes (`[ApiController]`, `[HttpGet("/x")]`) |
| gRPC | `@GrpcService` classes are tagged; no clients | none |

**You will not get** (Kotlin): imports, gRPC clients. **(C#)**: a `using` graph, gRPC, minimal APIs
(`MapGet`), MassTransit, Azure Service Bus, RabbitMQ, DI-registered handlers, and the `.csproj` /
NuGet / solution graph.

Consequence: `find_dependents` cannot answer "who imports this" for Kotlin or C# files, and
gRPC tracing (`analyze_grpc`) will not list a C# service or client. The `.proto` is still the
source of truth for the contract, and a Java or Go caller of a C# service is still found. (In the
`online-boutique` golden file, `CartService` is a C# server, but its scored edges are
`caller -> CartService`, written from client construction sites in the callers; no C# gRPC extraction
is involved in them.)

### 2.8 Ruby, PHP, Swift, Scala (level 1)

Declarations only, plus one routing idiom each:

| | Declarations | Route idiom |
| :--- | :--- | :--- |
| Ruby | classes, modules (as interfaces) | Rails controller actions and `config/routes.rb` DSL calls |
| PHP | classes, interfaces | Symfony `#[Route(...)]`, Laravel `Route::get(...)` |
| Swift | classes/structs/extensions, protocols | Vapor `app.get("/path") { ... }` |
| Scala | classes/objects, traits | Akka HTTP `path("segment") { get { ... } }` |

No imports, no gRPC, no events. These languages are useful to `smart_search` and, for the route
idioms, to route lookup; they are not part of the cross-service graph beyond that.

---

## 3. Contract and document formats

### 3.1 Protobuf

Grammar: `tree-sitter-proto`. Services, methods, messages and `import` dependencies. It is the
source of truth for `analyze_grpc`, which also compares a `.proto` against a Git base for
wire-format breaking changes: field number reuse, incompatible type changes, deleted fields
without `reserved`, enum number reuse and deletion, with a documented compatibility table
([mcp-tools](mcp-tools.md#wire-format-check)). Files under 1.5 MB are accepted (the schema size
limit is higher than for source files). `[engines.contracts.grpc] proto_dirs` scopes which
`.proto` files count.

### 3.2 OpenAPI, AsyncAPI, Spring properties

Read without tree-sitter: YAML is streamed event by event with budgets (an alias bomb ingests
nothing). OpenAPI contributes paths, AsyncAPI channels (topics), Spring `application*.yml` and
`.properties` contribute properties used to resolve topic placeholders; keys that look like secrets
are masked. `[engines.contracts.openapi|asyncapi|spring]` scope which files count.

### 3.3 Markdown

Split into sections for `search_docs`, with a per-file memory budget, YAML/TOML frontmatter kept out
of the body, and prompt-injection phrases filtered. See [mcp-tools](mcp-tools.md#tool-5-search_docs).

---

## 4. What every level shares

- **Pattern-based.** A framework is recognised by a convention in the syntax tree, not by
  resolving types. A project that wraps a library behind its own helper (an outbox, an event bus,
  a base controller) is invisible until you declare it with `[[engines.contracts.patterns]]`.
- **Confidence is explicit.** Edges are `exact`, `heuristic` or `ambiguous`. A declarative
  pattern produces `heuristic` edges unless it sets `confidence = "exact"`.
- **Zero means "not statically resolved in the indexed roots".** See the README for the
  wording and [mcp-tools](mcp-tools.md#unresolved-flow-notes).
- **Bounded input.** A file over 384 KB (1.5 MB for `.proto`, protobuf-generated stubs and files named exactly `openapi`/`asyncapi` (`.yaml`/`.json`)), with a line
  over 1,024 bytes, a null byte in the first 4 KB or nesting deeper than 64 is not parsed; it is
  listed by `mesh-mcp doctor` and named in tool answers whose scope contains it.
- **Generated code counts as code.** A generated stub inside a root is indexed unless excluded.
  Stubs outside the workspace are not seen.

## 5. Adding depth

To raise a language: [`development.md`](development.md#4-adding-a-tree-sitter-language) for a
new grammar, and the JavaScript/TypeScript subsection for adding a library or framework to the
rule tables. A new idiom needs a unit test through `PolyglotIndexer::index_file` so the dispatch
wiring is covered, and a note of its known limits in `CHANGELOG.md`.
