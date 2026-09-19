# Knobyte CLI Reference Manual

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

This manual documents all commands, subcommands, and flags available in the `knobyte` command-line interface.

---

## Global Usage

```bash
knobyte [OPTIONS] [COMMAND]
```

### Options
- `-h, --help`: Print help information.
- `-V, --version`: Print version information.

If no subcommand is passed, `knobyte` automatically boots the local **Project Hub** web server on `127.0.0.1:3000`.

---

## Command Reference

### `setup`
Initializes the `.knobyte/` scaffold in the current repository.
```bash
knobyte setup [OPTIONS]
```
- `--cli`: Run in interactive terminal mode without opening browser.
- `--dry-run`: Preview files to be created without writing to disk.
- `--mode <MODE>`: Scaffold mode (`code-repo` [default], `monorepo`, `docs-only`).

---

### `check`
Evaluates grounding drift score by comparing AST node bodies against documentation assertions.
```bash
knobyte check [OPTIONS]
```
- `--json`: Output drift report as formatted JSON.
- `--quiet`: Print summary score line only.
- `--fix`: Automatically relocate moved grounding anchors in frontmatter and inline comments.

---

### `sync`
Detects and relocates grounding anchors (`grounds_to` and `<!-- kb-ground: <id> -->`) across scaffold files when code symbols move across files.
```bash
knobyte sync [OPTIONS]
```
- `--dry-run`: Preview grounding relocations without modifying files on disk.
- `--warnings`: Include non-critical warning-level drift issues in the AI prompt plan.

---

### `graph`
Inspects, queries, and manages the deterministic Tree-sitter code graph.

#### `knobyte graph rebuild`
Parses all supported source files and rebuilds `.knobyte/graph.db` (automatically synchronizes with CozoDB).
- `--quiet`: Suppress verbose file scanning output.

#### `knobyte graph query <RELATION> <TARGET>`
Executes deterministic structural queries.
- `RELATION`: `where-defined`, `who-calls`, `who-imports`.
- `TARGET`: Symbol or function name.

#### `knobyte graph scope "<TASK>"`
Discovers the bounded context neighborhood of symbols relevant to a coding task.
- `--depth <N>`: Traversal hop depth (default: 2).

#### `knobyte graph status`
Displays node count, edge count, and schema status.
- `--json`: Output as JSON.

---

### `wiki`
Full-text search and management of Markdown documentation.

#### `knobyte wiki rebuild-index`
Indexes all Markdown files in `.knobyte/` into `.knobyte/wiki.db` using SQLite FTS5.

#### `knobyte wiki query "<QUERY>"`
Searches wiki entities using full-text keywords.
- `--limit <N>`: Maximum results (default: 10).

#### `knobyte wiki show <ID>`
Displays a specific wiki entity by ID (e.g. `kb_stack`).

#### `knobyte wiki validate`
Validates entity schemas and detects broken or dangling link references.

---

### `cozo`
Interacts with the embedded CozoDB Datalog and HNSW vector engine.

#### `knobyte cozo search "<QUERY>"`
Performs 128-dimensional dense HNSW vector similarity search over code and wiki entities.
- `--target <TARGET>`: `code` (default) or `wiki`.
- `-k, --limit <K>`: Number of nearest neighbors to return (default: 5).

#### `knobyte cozo query "<SCRIPT>"`
Executes raw Datalog queries directly against the database.
```bash
knobyte cozo query "?[kind, count(id)] := *code_nodes{kind, id}"
```

#### `knobyte cozo pagerank`
Computes PageRank graph centrality across the entire codebase to identify core functions.
- `--damping <FLOAT>`: PageRank damping factor (default: 0.85).
- `--iterations <N>`: Number of iterations (default: 20).

#### `knobyte cozo shortest-path <SOURCE_ID> <TARGET_ID>`
Computes the shortest call or import path between two symbols using BFS.

#### `knobyte cozo sync`
Forces bidirectional synchronization of SQLite `graph.db` and `wiki.db` into `cozo.db`.

---

### `log`
Appends or reads durable project events and architectural decisions.
```bash
knobyte log "<MESSAGE>" [OPTIONS]
```
- `--kind <KIND>`: Event kind: `decision`, `discovery`, `note`, `risk`, `todo` (default: `note`).
- `--tags <TAGS>`: Comma-separated tags (e.g. `--tags "auth,security"`).
- `--files <FILES>`: Comma-separated related file paths.
- `--read`: Read recent log entries instead of appending.
- `--limit <N>`: Number of entries to read (default: 10).

---

### `timeline`
Searches project history and event logs.
```bash
knobyte timeline [OPTIONS]
```
- `--query <TEXT>`: Filter by keyword in event summary.
- `--kind <KIND>`: Filter by event kind (`decision`, `discovery`, `note`, `risk`, `todo`).
- `--file <PATH>`: Filter by associated file path.
- `--since <ISO8601>`: Filter events after timestamp.
- `--limit <N>`: Number of records to return (default: 20).

---

### `relay`
Manages team and agent context handoffs (Schema v4).

- `knobyte relay list`: List active and completed handoff relays.
- `knobyte relay publish <DRAFT_ID>`: Publish a local draft to the team.
- `knobyte relay acknowledge <ID> [--member <ID>]`: Claim a relay to resume work.
- `knobyte relay close <ID> [--member <ID>]`: Close a finished relay.
- `knobyte relay draft save`: Save a local relay draft.
- `knobyte relay draft list`: List local drafts.

---

### `inbox`
Manages proposals for additions or corrections to project memory.

- `knobyte inbox draft list`: List pending local drafts.
- `knobyte inbox draft save`: Create or update a proposal draft.
- `knobyte inbox publish <DRAFT_ID>`: Publish draft for team review.
- `knobyte inbox proposals`: List active proposals.

---

### `member`
Manages canonical team contributors and local session attribution.

- `knobyte member list`: List registered contributors.
- `knobyte member current`: Show active local identity.
- `knobyte member select <ID>`: Switch local active contributor.
- `knobyte member clear`: Clear local identity selection.

---

### `hub`
Starts the local Project Hub web interface.
```bash
knobyte hub [OPTIONS]
```
- `--port <PORT>`: HTTP port (default: 3000).
- `--host <HOST>`: Bind host (default: `127.0.0.1`).
- `--no-open`: Do not automatically open the browser.

---

### `mcp`
Runs the native Model Context Protocol (MCP) server.
```bash
knobyte mcp [OPTIONS]
```
- `--sse`: Run remote Server-Sent Events (SSE) server.
- `--stdio`: Run local Stdio transport.
- `--port <PORT>`: SSE port (default: 3001).
- `--host <HOST>`: SSE host (default: `127.0.0.1`).

---

### `doctor`
Executes environment, index, and database health diagnostics.
- `--json`: Output diagnostic report as JSON.

---

### `capabilities`
Machine-readable capability discovery for AI agents.
- `--json`: Output capability schema as JSON.
