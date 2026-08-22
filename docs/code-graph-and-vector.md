# Code Graph & Vector Intelligence

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

Knobyte pairs a deterministic Abstract Syntax Tree (AST) code graph with an embedded CozoDB store for Datalog queries and vector similarity search. This lets coding agents combine precise structural references with similarity search, all computed locally.

---

## 1. Deterministic AST Code Graph (Tree-sitter)

### Building and maintaining the graph

| Command | Purpose |
|---|---|
| `knobyte graph` | Build the graph for the current project, or any directory with `--root` (no scaffold needed) |
| `knobyte graph refresh` | Incremental: re-extract only changed files and publish atomically |
| `knobyte graph rebuild` | Full rebuild into an isolated candidate, published atomically |
| `knobyte graph status` | Read-only health: `fresh`, `stale`, `degraded`, `corrupt`, `rebuild_required` or `missing` |
| `knobyte graph repair` | In-place repair: WAL recovery, index and FTS rebuild, schema upgrade, dangling rows |

`refresh`, `rebuild` and `repair` take a lock. `--lock-timeout <SECONDS>` waits for a concurrent
run instead of failing.

```text
$ knobyte graph status
=== Code Graph Status ===
Graph status: fresh
Last successful index: 2026-10-02T02:01:14.361976+00:00
Nodes: 12  Edges: 19  Files: 3  Unresolved refs: 3
Schema: v6  Extractor: knobyte-extract-5
Sources: 0 changed (0 added, 0 modified, 0 deleted)
Parse health: 3 ok, 0 partial, 0 failed
```

`.knobyte/config.json` can tune the graph:

- `graph.ignore` adds extra ignore globs;
- `graph.max_file_bytes`, `graph.max_files` and `graph.max_total_bytes` bound the corpus.

Knobyte compiles Tree-sitter grammars into the binary for:
- **Rust** (`tree-sitter-rust`)
- **TypeScript & TSX** (`tree-sitter-typescript`)
- **JavaScript & JSX** (`tree-sitter-javascript`)
- **Python** (`tree-sitter-python`)
- **C#** (`tree-sitter-c-sharp`): namespaces, classes, structs, records, interfaces, enums,
  delegates, methods, constructors, operators, properties, fields, constants and parameters;
  `using` imports, calls, instantiations, attributes and inheritance. Overloads are qualified by
  parameter types (`Ns.Type.Find(int)`).
- **Swift** (`tree-sitter-swift` 0.6, the last grammar with the ABI Knobyte's Tree-sitter loads):
  classes and actors (`class`), structs, enums with their cases (`enum_member`), protocols
  (`interface`) with their requirements, typealiases and associated types, functions, methods,
  `init` / `deinit` / `subscript`, stored and computed properties (instance and static), global
  `let` / `var`, parameters, nested types and generics; `import` / `@testable import` (module
  nodes), calls, initializer calls `T(..)` (`instantiates`), payload enum cases `E.c(..)`
  (`references`), inheritance, conformances and `override` members (`overrides`). Visibility is
  `open` / `public` / `internal` / `fileprivate` / `private`; docstrings come from `///` and
  `/** */`. Extensions are not nodes: their members belong to the extended type (in any file),
  and `extension T: P` is `T -implements-> P`. A class's first inherited type is `extends`
  unless it resolves to a protocol. Overloads are qualified by argument labels
  (`Circle.move(to:)`, `Circle.move(by:_:)`) and calls pick the overload their labels name.
  Names resolve within the Swift module — a SwiftPM target (`Sources/<Target>/`,
  `Tests/<Target>/`) or, outside SwiftPM, the top-level folder — without imports, plus the
  `public` / `open` declarations of in-repository modules a file imports (all of them through
  `@testable import`). Receiver types come from `let x = T(..)`, `let x: T`, parameter and
  property types, `T(..).m()` and `super` (the class's first inherited type). Code the 0.6
  grammar does not know (some Swift 6 syntax such as `nonisolated(unsafe)`) parses as `partial`.

Framework route resolvers turn HTTP routes into `route` nodes named `METHOD /path`, each linked
to its same-file handler (`route -references-> handler`): Express, FastAPI, Flask, NestJS and
Next.js (App Router `route.*` files and `pages/api/**`). Express routes carry their full mounted
path: routers mounted in routers (`api.use('/v1', v1); app.use('/api', api)`) compose across any
number of files and levels, through `import`, `require('./x')` and re-exports, including mounts
onto an imported router. `router.route('/x').get(..).post(..)` chains and array mounts
(`app.use('/admin', [auth, adminRouter])`, `app.use(['/a', '/b'], r)`) are recognised. A router
mounted at several paths gets one route node per path; mount cycles are cut. Python
`include_router` prefixes are not followed.

TypeScript/JavaScript resolution approximates the type checker syntactically: `tsconfig.json` /
`jsconfig.json` `baseUrl` and `paths` (JSONC, relative `extends`), barrel re-export chains
(`export { x } from`, `export * from`), `type A = B` aliases (`aliases` edges, and type
references reaching the aliased type), overload signature lists and declaration headers as
signatures. Generic instantiation, conditional and mapped types, unions, declaration merging,
`node_modules` / package `exports` resolution and project references are beyond it, so such
calls stay unresolved or heuristic rather than guessed (the optional type-checker mode below
closes these gaps).

Method calls bind through a bounded, source-only receiver-type inference (provenance
`ts-inference`, confidence 0.9, below the checker's 1.0). A receiver's type comes from explicit
annotations (variables, parameters, destructured typed parameters, class and interface fields,
getters), constructor parameter properties and `this.x = ..` constructor assignments (also in
JavaScript), `new C()` initializers, `as T` / `<T>` casts, annotated return types of functions
and methods (`const s = makeSvc(); s.run()`, `await` unwrapping `Promise<T>`, `this` return
types), module-level variables (`export const svc = new Svc()` imported elsewhere), `this` /
`super` in classes and arrow callbacks, static calls `C.m()` and a type parameter's `extends`
constraint. Chains follow at most four call links (`a.b().c().d()`). Type names bind only through
lexical scope and explicit imports (aliased, default, barrel and tsconfig-`paths` imports and
type aliases included); members are looked up on the class and its `extends` chain across files,
or on an interface and the interfaces it extends (interface-typed receivers bind to the interface
member). `T | null | undefined` counts as `T`; any other union, an untyped or loop variable, an
unresolvable base class, or two candidate members leaves the call to the conservative resolver;
nothing is guessed by name.

Measured against the type-checker mode (same build, `--ts-compiler`): on the
`tests/fixtures/graph/ts_infer` suite source-only resolution matches 63 of 66 checker-resolved
call edges (95.5%; the rest are two union-typed receivers the checker pins to their first member and a
`for..of` element), with no inferred edge contradicting the checker. On a 30.7k-edge TypeScript
codebase it matches 29,537 of 30,693 (96.2%, up from about 82% without inference) and on a
42.7k-edge repository 40,204 of 42,731 (94.1%, up from about 80%), again with no contradicting
inferred edge. Reproduce with `KNOBYTE_TS_INFER_PROJECT=<dir> cargo test --test
graph_ts_infer_test -- --ignored --nocapture`.

Source-only TS/JS extraction also records module-level `const` / `let` / `var` bindings as
`constant` / `variable` nodes (calls in their initializers are made by the binding), anonymous
callbacks as their own `function` nodes named `<callback:callee[index]>` (for example
`<callback:items.map[0]>`; repeats in a file are qualified `#2`, `#3`, ...; an anonymous default
export is named `default`), resolves `export { default as X } from './m'` and default imports to
the declaration `m` exports as default, and qualifies namespace members (`NS.inner`,
`A.B.f`) so `NS.inner()` resolves, in the same file or through `import { NS }`.

#### Optional TypeScript type-checker mode

The default stays pure Rust and needs no runtime. A project can opt in to the TypeScript type
checker, in `.knobyte/config.json`:

```json
{ "graph": { "typescript": { "compiler": "tsc" } } }
```

or for one run with `--ts-compiler` on `knobyte graph`, `knobyte graph refresh` and
`knobyte graph rebuild`. `"compiler"` may be `"source"` (the default) or `"tsc"`; `"typescript"`
and `"compiler"` are accepted as synonyms for `"tsc"`. Optional keys:

- `typescript_path`: a `typescript` package directory;
- `node_path`: the Node binary (default: `node` on `PATH`);
- `timeout_secs`: default 300.

Knobyte uses an **existing** Node and `typescript` package and never installs either. It looks for
the package in this order:

1. the project's own `node_modules/typescript`;
2. `graph.typescript.typescript_path`, if set (when set, nothing further is tried);
3. read-only discovery of global installs:
   - `$(npm root -g)/typescript`;
   - the Node prefix (`<prefix>/lib/node_modules/typescript`);
   - Homebrew (`$(brew --prefix)/opt/typescript/libexec/lib/node_modules/typescript`, for example
     `/opt/homebrew/opt/typescript/libexec/lib/node_modules/typescript`);
   - and on macOS and Linux, the same paths under `/opt/homebrew` and `/usr/local`.

A directory only counts if it holds a `package.json` named `typescript` and a
`lib/typescript.js`.

When Node and the package are found, the build runs a helper script shipped
inside the knobyte binary (written to `.knobyte/ts-compiler/`, which is git-ignored). It loads
that compiler, builds one program per tsconfig project (project references resolved to their
sources, `include` / `exclude`, several programs per repository with the most specific config
owning a file, files outside every tsconfig in an inferred `allowJs` program, package `exports`
and `node_modules` resolution by the compiler itself), and reports:

- resolved call targets, including methods on typed receivers (`make().run()`, `x.run()` where
  `x: Worker`), `new C().m()`, `super.m()`, aliased imports and overloaded calls (landing on the
  implementation);
- checker-rendered signatures (`pick(x: string): string; pick(x: number): number` for an
  overload set) and return types, replacing the declaration headers;
- resolved `type A = B` aliases.

These become `calls` / `instantiates` / `aliases` edges with confidence 1.0 and provenance
`typescript-compiler`; a call the checker cannot pin to exactly one indexed declaration keeps its
source-only resolution. Nothing is ever installed or downloaded and the helper does no network
I/O. If Node or `typescript` is missing, or the helper fails or times out, knobyte prints one
warning and builds the source-only graph; the mode never fails a build. The build summary
(`typescript_compiler` in `--json` output) and the `typescript_compiler_status` graph metadata
say which mode produced the graph.

Checker results are cached in `.knobyte/ts-compiler/facts-cache.json` per file content hash,
under a digest of the helper, the TypeScript version and every tsconfig / jsconfig / package
manifest / lockfile. A refresh re-checks the changed files plus the transitive closure of files
that import them (by the checker's own module resolution) and reuses the rest; an added or
deleted file, a changed script or declaration file visible without an import, or any
configuration change re-checks the whole corpus. Switching the mode makes the next
`graph refresh` re-resolve even when no source changed.

It also extracts lightweight symbols from SQL schema files (tables, indexes, triggers), API endpoints from OpenAPI-style JSON/YAML, and ADR Markdown documents.

### Extracted Entities
- **Nodes**: Files, functions, methods, structs, enums, traits, impls, classes, interfaces, and type aliases. Each node records its qualified name, signature, start/end lines, body hash, and container.
- **Edges**: `calls`, `imports`, and trait/implementation relationships (`implements`, `impl_of`, `calls_trait_method`, `possible_call`).

### Call and Import Resolution
- Calls are resolved through lexical scope and imports. A call that cannot be attributed to exactly one symbol is kept as an *unresolved reference* (counted in `knobyte graph status`) instead of being guessed.
- Imports are parsed structurally for Rust (`use`), TypeScript/JavaScript (`import`/`require`), and Python (`import`/`from ... import`), so `who-imports` can answer by symbol or module.

### Readable References
Symbols are identified to people and agents by readable references of the form `kind:path:qualified_name`, for example `function:src/auth.rs:login` or `method:src/auth.rs:Session::is_valid`. The same form is used for documentation groundings.

### Storage Architecture
- Code graph data is stored locally in `.knobyte/graph.db` using **SQLite with Write-Ahead Logging (WAL)**.
- Indexes on node name, qualified name, file, and kind serve fast symbol lookups; edge indexes on `(source, kind)` and `(target, kind)` serve forward and reverse traversal (`who-calls`).
- Grounding baselines are committed in the Markdown (`grounds_to` `body_hash` / `fingerprint`
  and `<!-- kb-ground: ref #hash -->`; see [Grounding and drift](grounding-and-drift.md)). The
  `_knobyte_grounded_source` and `_knobyte_grounded_neighbors` tables are a local cache of the
  grounded bodies and their callers and callees, preserved across `graph rebuild`.
- `code_chunks` (FTS5) indexes overlapping 80-line source windows for scope's full-text channel.
- `node_minhash` / `node_lsh` hold MinHash sketches (64 values over identifier-erased token
  trigrams) and LSH buckets. When a grounded symbol no longer resolves and neither its exact
  body hash nor its readable reference finds it, the reconciler decides MOVED / AMBIGUOUS / GONE
  from body similarity and caller/callee continuity; `knobyte sync` relocates such moves, and a
  rename decided by neighbours is reported as `GROUNDING_MOVED_BY_NEIGHBORS`.

### Structural queries

`knobyte graph query <relation> <target>` supports `where-defined`, `who-calls`, `what-calls`
and `who-imports`:

```text
$ knobyte graph query where-defined login
function login (src/auth.rs:17:0)

$ knobyte graph query who-calls validate_token
method Session::is_valid (src/auth.rs:8:4)

$ knobyte impact validate_token
Impact radius for 'validate_token': 2 affected nodes (depth 3)
  * function validate_token (src/auth.rs)
  [1] method Session::is_valid (src/auth.rs) via calls
```

`knobyte graph scope "<task>"` returns the ranked files, source, directed flows and facts for a
task in one response. `--wiki` attaches grounded wiki entities, and `--hybrid` re-ranks with
vector similarity:

```text
$ knobyte graph scope "session token validation"
== src/auth.rs
 1: use crate::password::verify_password;
 ...
13: pub fn validate_token(token: &str) -> bool {
14:     !token.is_empty()
15: }
...
flow: is_valid -> validate_token
function validate_token (src/auth.rs:13)
struct Session (src/auth.rs:3)
method Session::is_valid (src/auth.rs:8)
property Session::token (src/auth.rs:4)
-- status ok, evidence moderate, 4 node(s), 1 file(s)
```

How scope ranks, in order:

1. **Channels.** Whole-task BM25 over declarations, exact explicit identifiers, one search per
   query concept, one per adjacent concept pair, source-chunk full text, and path matches, fused
   by reciprocal rank. `--hybrid` adds Cozo vector similarity as one more channel (off by
   default).
2. **Semantic phrase bridges.** When an adjacent query phrase (`cache invalidation`) matches the
   name or signature of both ends of a trusted call or reference *across files*, the
   destination file is reserved (at most two). Comment-only, same-file and low-confidence
   matches never qualify.
3. **Source regions and callsites.** Each matching source window is tied to the declarations it
   overlaps. Trusted calls made inside the window pass the window's evidence to their callees.
4. **Seeds and expansion.** Phrase sources, whole-query call pairs, callback owners and callsite
   regions seed first, then one seed per file. Expansion follows trusted typed edges for two
   hops.
5. **Files.** Explicit identifiers pin a file. Phrase destinations come next, then a source
   floor (direct text hits plus the best callsite destination, up to three files) and files with
   strong compound declaration names. The fused score fills the rest. The primary flow can then
   replace the weakest unprotected file.
6. **Declarations.** Explicit identifiers, call-pair endpoints, the best whole-query
   declaration, distinct intent facets and each file's best source-aligned declaration are
   reserved before per-file representatives.
7. **Flows.** Directed call paths within an eight-step budget. Query-region callsites, callback
   provenance (named-owner repair) and call-pair callback completions are reserved first.

Source is then admitted in phases. Global reservations come first: the dominant exact lookup,
the first flow's terminal target and one corroborated natural-language answer. Then come the
strongest file's answers, answers from later files in rotation, one compact range per file,
further anchors and finally optional bodies. Each source line is emitted at most once. A
complete declaration (up to 160 lines) is never cut to a prefix. Source gets 75% of the payload
after framing and flows reserve up to 15%. Capacity a phase leaves unused goes to deferred
source.

`knobyte graph get <id-or-ref…> --source` prints node bodies. `--max-lines` caps them at 400
lines.

### Agent protocol v3

`graph query`, `graph scope`, `graph get` and `impact` can emit JSONL records for agents
(`--jsonl`). Any budget flag also selects this protocol. The budget flags are:

- `--detail minimal|standard|source`
- `--max-nodes`, `--max-files`, `--max-flow-steps`
- `--max-output-tokens` (estimated at 4 characters per token)
- `--max-source-lines`
- `--fingerprint`, which attaches body hashes and MinHash fingerprints for grounding work

The MCP graph tools accept the same budgets.

---

## 2. CozoDB Vector & Datalog Engine

In addition to SQLite, Knobyte embeds **CozoDB**, a relational, graph, and vector database, on Sled storage in `.knobyte/cozo.db`. `knobyte graph rebuild` and `knobyte wiki rebuild-index` keep it in sync; `knobyte cozo sync` forces a sync.

### Schema Relations in `cozo.db`

`D` is the embedding dimension of the active backend (128 for `hashed`, 256 for the default Model2Vec model).

```cozodatalog
:create code_nodes {
    id: String =>
    file_path: String,
    kind: String,
    name: String,
    start_line: Int,
    end_line: Int,
    body_hash: String,
    embedding: <F32; D>,
    qualified_name: String default ""
}

:create code_edges {
    source_id: String,
    target_id: String,
    kind: String =>
    file_path: String
}

:create wiki_entities {
    id: String =>
    title: String,
    path: String,
    tags: [String],
    summary: String,
    embedding: <F32; D>
}

:create embedding_meta {
    relation: String =>
    embedder_id: String,
    dim: Int
}

::hnsw create code_nodes:node_vec {
    dim: D,
    fields: [embedding],
    distance: Cosine,
    ef_construction: 64,
    m: 16
}
```

`wiki_entities:wiki_vec` is the matching HNSW index for wiki entities.

---

## 3. Local Embedding Backends

Embeddings are always computed on your machine. Choose the backend per project; it is saved in `.knobyte/config.json`.

### `hashed` (default, 128-dim)
A deterministic hashed lexical embedding that needs no model and no download:
- **Inputs**: For code, the symbol name and qualified name (weighted highest), signature, docstring, kind and path, and identifier tokens from the body. For wiki entities, title, summary, type, and body.
- **Token Extraction**: Splits identifiers across `snake_case` and `camelCase`, keeping compound identifiers too.
- **Feature Hashing**: Hashes tokens, token bigrams, and character trigrams with SHA-256 into 128 signed buckets, then L2-normalizes the vector.

It matches shared words and identifiers; synonyms with no shared tokens are not related.

### `model2vec` (optional, 256-dim by default)
A local [Model2Vec](https://github.com/MinishLab/model2vec) static embedding model, by default `minishlab/potion-base-8M`, which captures semantic similarity. The model is downloaded only when you run `knobyte cozo model pull`, into `~/.knobyte/models` (override with `KNOBYTE_MODELS_DIR`).

```bash
knobyte cozo model pull                 # explicit download from huggingface.co
knobyte cozo model use model2vec        # fails with instructions if the model is not pulled yet
knobyte cozo model status               # backend, dimension, and which embedder built each index
knobyte cozo model use hashed           # switch back
```

After a backend change, vector indexes are re-embedded on the next `knobyte cozo sync` or search.

### Vector Search CLI

```bash
# Search code nodes
knobyte cozo search "authentication bearer token" --target code --k 5

# Search wiki markdown entities
knobyte cozo search "state management and caching" --target wiki --k 5
```

Example output (hashed backend):

```text
$ knobyte cozo search "token validation" --k 3
Vector search results for 'token validation' (target: code, embedder: hashed-v1):
1. function:src/auth.rs:validate_token [score: 0.569, dist: 0.431]
   ...
2. property:src/auth.rs:Session::token [score: 0.419, dist: 0.581]
   ...
3. method:src/auth.rs:Session::is_valid [score: 0.363, dist: 0.637]
   ...
```

A search returns `--k` matches whenever `--k` candidates clear the relevance floor:

- The HNSW index is over-fetched (`max(4k, k + 16)` candidates, `ef` at least twice that), then
  external `module` nodes are skipped and the floor applied. When the index returned fewer
  candidates than it should, an exact cosine scan fills the gap.
- The floor is a score (`1 - cosine distance`) of 0.20 by default. `--min-score <0-1>` changes
  it, and `--min-score 0` disables it.
- The floor never cuts results silently. When it removes some of the `k` nearest candidates,
  the search says how many:

```text
$ knobyte cozo search "render pipeline" --k 5
Vector search results for 'render pipeline' (target: code, embedder: hashed-v1):
1. function:src/ui/canvas.rs:render_canvas [score: 0.412, dist: 0.588]
   ...
[info] 4 of the 5 nearest candidate(s) scored below the relevance floor 0.20 and were left out; lower it with --min-score (0 disables it).
```

With no hits at all, it prints `No matches above the relevance floor.` (or, for an empty index,
a hint to run `knobyte cozo sync`). `--json` prints the matches as an array and sends the floor
note to stderr. `knobyte_vector_search` (MCP) and Hub search use the same search; the MCP tool
returns `k`, `minScore`, `belowFloor` and a `message`, and accepts `minScore`.

`knobyte cozo sync` fills `cozo.db` explicitly:

```text
$ knobyte cozo sync
[ok] Synchronized SQLite storage to CozoDB:
  - Code Nodes:  12 with 128-dim embeddings
  - Code Edges:  19
  - Wiki Pages:  12 with 128-dim embeddings
  - Embedder:    hashed-v1
  - Storage:     …/.knobyte/cozo.db
```

---

## 4. Graph Algorithms

### PageRank Centrality
Computes the structural importance of code symbols based on dependency topology:
```text
$ knobyte cozo pagerank --iterations 20 --damping 0.85    # --theta is an alias for --damping
PageRank Centrality Scores (damping: 0.85, iterations: 20):
1. function:src/crypto.rs:verify_password - rank: 0.067205
2. function:src/auth.rs:validate_token - rank: 0.035201
3. function:src/auth.rs:login - rank: 0.031689
...
```

### Shortest Path
Finds the shortest dependency route between two symbols. Endpoints may be symbol names, readable refs, or node IDs:
```text
$ knobyte cozo shortest-path login verify_password
Shortest path from 'login' to 'verify_password' (length 2):
  function:src/auth.rs:login
  -> function:src/crypto.rs:verify_password

$ knobyte cozo shortest-path function:src/auth.rs:login function:src/crypto.rs:verify_password
```

### Custom Datalog Queries
Queries are read-only unless you pass `--mutable`:
```bash
knobyte cozo query "?[kind, count(id)] := *code_nodes{kind, id}"
knobyte cozo query "?[relation, embedder_id, dim] := *embedding_meta{relation, embedder_id, dim}"
```
