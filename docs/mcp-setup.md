# MCP Server

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

Knobyte ships a Model Context Protocol (MCP) server in the same binary. It runs over **stdio**
or over HTTP, where one server offers **Streamable HTTP** (`/mcp`), the older **HTTP + SSE**
transport (`/sse` + `/messages`), or both (the default). Agents get the code graph, vector search,
wiki, drift checks and team memory as 38 tools. The server always works on the project it was
started in.

---

## Starting the server

### Stdio

For clients that start Knobyte as a subprocess (Claude Desktop, Cursor, Cline and others):

```bash
knobyte mcp --stdio
```

Start it from the repository root, or set the client's working directory to it. The server serves
the project it finds there.

### HTTP (Streamable HTTP and SSE)

For clients that connect over HTTP, or for several agents sharing one server:

```bash
knobyte mcp                 # both HTTP transports (defaults: --host 127.0.0.1 --port 3005)
knobyte mcp --http          # Streamable HTTP only (/mcp)
knobyte mcp --sse           # legacy HTTP + SSE only (/sse + /messages)
```

`--stdio`, `--http` and `--sse` are mutually exclusive. With `--http` the SSE endpoints answer
`404`, and with `--sse` `/mcp` does; `GET /` lists only the live transports and endpoints. The
server prints the live endpoints at startup:

```text
[mcp] Knobyte Remote MCP Server listening on http://127.0.0.1:3005
   Transports: streamable HTTP (/mcp) and legacy SSE (/sse + /messages)
   - Streamable MCP:   http://127.0.0.1:3005/mcp
   - SSE Endpoint:     http://127.0.0.1:3005/sse
   - Messages:         http://127.0.0.1:3005/messages
   - Dashboard:        http://127.0.0.1:3005/dashboard
   - Health check:     http://127.0.0.1:3005/health
```

| Endpoint | Purpose |
|---|---|
| `POST /mcp` | Streamable HTTP (off with `--sse`). The session is identified by the `Mcp-Session-Id` header, and `DELETE /mcp` ends it. |
| `GET /sse` | SSE stream (off with `--http`). The first event names the messages URL for this session. |
| `POST /messages?sessionId=…` | JSON-RPC messages for an SSE session (off with `--http`). It answers `202 Accepted`, and the reply arrives on the SSE stream. |
| `GET /dashboard` | A small inspector page. |
| `GET /health` | Liveness check: `{"service":"knobyte-mcp","status":"ok","version":"0.9.3"}`. |

---

## Security

- **Loopback by default.** The HTTP server binds to `127.0.0.1` unless you pass `--host`.
- **Host and Origin checks.** Every request's `Host` and `Origin` headers are validated. This
  blocks DNS rebinding and cross-site browser requests.
- **Bearer tokens.** `--token <secret>` or `KNOBYTE_MCP_TOKEN` makes every request, including
  `/health`, require `Authorization: Bearer <secret>`. Clients that cannot set headers may pass
  `?token=<secret>` instead, but only on `GET /sse`, `POST /messages` and the dashboard. The
  messages URL handed out over SSE carries the token automatically.
- **Non-loopback binds always need a token.** If you bind elsewhere without supplying one,
  Knobyte generates a token and prints it to stderr:

```text
$ knobyte mcp --host 0.0.0.0
[mcp] Bound to non-loopback address 0.0.0.0; bearer authentication is required.
   Generated token (set KNOBYTE_MCP_TOKEN to choose your own):
   Authorization: Bearer <generated token>
```

- **No approval tools.** No tool publishes drafts, approves or rejects Inbox proposals, or
  claims and closes Relays. Agents prepare drafts and people decide.

---

## Client configuration

`knobyte setup --tools …` writes agent instruction files but does not register the MCP server
with your client. Add it yourself.

### Claude Code

```bash
claude mcp add knobyte -- knobyte mcp --stdio
```

### Claude Desktop

Edit `claude_desktop_config.json`: on macOS `~/Library/Application Support/Claude/`, on Windows
`%APPDATA%\Claude\`.

```json
{
  "mcpServers": {
    "knobyte": {
      "command": "/usr/local/bin/knobyte",
      "args": ["mcp", "--stdio"]
    }
  }
}
```

### Cursor, Windsurf, Cline and other JSON-configured clients

Stdio:

```json
{
  "mcpServers": {
    "knobyte": { "command": "knobyte", "args": ["mcp", "--stdio"] }
  }
}
```

Streamable HTTP (server started with `knobyte mcp` or `knobyte mcp --http`), with a token if
the server requires one:

```json
{
  "mcpServers": {
    "knobyte": {
      "url": "http://127.0.0.1:3005/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

Clients that only speak the older SSE transport connect to `http://127.0.0.1:3005/sse` (server
started with `knobyte mcp` or `knobyte mcp --sse`):

```json
{
  "mcpServers": {
    "knobyte": { "type": "sse", "url": "http://127.0.0.1:3005/sse?token=<token>" }
  }
}
```

Claude Code can also connect over HTTP:
`claude mcp add --transport http knobyte http://127.0.0.1:3005/mcp`.

---

## Operating rules sent on connect

On `initialize` the server sends instructions that tell the agent to:

1. call `knobyte_session_start` first;
2. read `context/stack.md`, `AGENTS.md` and `ROUTER.md` before writing code;
3. use `knobyte_graph_query` before modifying shared code;
4. checkpoint with `knobyte_workstream_step_update`;
5. log decisions and risks with `knobyte_log`;
6. leave a `knobyte_relay_draft` at session end.

---

## Tools (38)

Parameters marked `*` are required. Every tool is read-only unless its description says it writes.

### Session and workstreams

| Tool | Parameters | What it does |
|---|---|---|
| `knobyte_session_start` | none | Entry point for a new or resuming agent. Returns project info, the current member, workstreams and the in-progress step, git HEAD and dirty files, the latest relay, recent decisions and risks, and the scaffold heartbeat. |
| `knobyte_workstream_step_update` | `status*`, `workstreamId`, `stepId`, `stepIndex`, `evidence`, `filesTouched` | Sets a workstream step's status, recorded as activity of the current actor and stamped with git HEAD and dirty files. It creates a missing step but never a workstream, and refuses archived or done workstreams. **Writes.** |
| `knobyte_file_context` | `filePath*` | Recorded events that reference a file, plus the code graph symbols defined in it. |

### Code graph

`graph_query`, `graph_scope` and `graph_get` also accept the protocol v3 budget parameters
`detail`, `max_nodes`, `max_files`, `max_flow_steps`, `max_output_tokens`, `max_source_lines`
and `fingerprint`.

| Tool | Parameters | What it does |
|---|---|---|
| `knobyte_graph_query` | `relation*`, `target*` | Structural lookup: where a symbol is defined, who calls it, what it calls, or who imports it. Includes index freshness. |
| `knobyte_graph_scope` | `task*`, `wiki`, `hybrid` | The symbols, definitions and code neighbourhood most relevant to a natural-language task. `wiki` attaches grounded wiki entities and `hybrid` re-ranks with vector similarity. |
| `knobyte_graph_get` | `ids*` | Source definitions and metadata for node ids or readable refs. |
| `knobyte_graph_status` | none | Node and edge counts, last indexed time and working-tree freshness. |

### Vectors and Datalog (CozoDB)

| Tool | Parameters | What it does |
|---|---|---|
| `knobyte_vector_search` | `query*`, `target` (`code`\|`wiki`), `k`, `minScore` | HNSW similarity search using the project's local embedding backend. Returns `k` matches whenever `k` candidates clear the relevance floor (`minScore`, default 0.2); `belowFloor` counts the nearest candidates the floor left out. Nothing leaves the machine. |
| `knobyte_cozo_datalog` | `script*`, `params` | Read-only CozoScript query over `code_nodes`, `code_edges`, `wiki_entities` and `embedding_meta`. Mutations are rejected. |
| `knobyte_cozo_pagerank` | `theta`, `iterations` | PageRank centrality over code dependency edges. |
| `knobyte_cozo_shortest_path` | `start*`, `target*` | Shortest dependency path between two symbols, given as node ids, names or readable refs. |

### Wiki

| Tool | Parameters | What it does |
|---|---|---|
| `knobyte_wiki_query` | `text*`, `type`, `status`, `topic`, `includeArchived`, `includeBody`, `limit`, `offset` | Ranked, paged search returning compact summaries. Empty text lists entities. |
| `knobyte_wiki_list` | `type`, `status`, `topic`, `includeArchived`, `includeBody`, `limit`, `offset` | Paged list of entities. Archived entities and shadowed duplicates are hidden by default. |
| `knobyte_wiki_show` | `id*` | Full details of one entity. |
| `knobyte_wiki_get` | `id*`, `includeBody`, `limit`, `relationsOffset`, `backlinksOffset` | One entity from a snapshot-bound read: relations, groundings with health, backlinks, source location and index state. Answers in the wiki envelope. |
| `knobyte_wiki_search` | `query`, `type`, `status`, `topic`, `includeArchived`, `limit`, `maxTokens`, `cursor` | Cursor-paged search over one index snapshot. A cursor is refused once the wiki changes (`REVISION_CONFLICT`). |
| `knobyte_wiki_neighborhood` | `id*`, `direction`, `relationTypes`, `depth`, `maxEntities`, `maxTokens`, `includeArchived` | Bounded breadth-first neighbourhood over typed relations. |
| `knobyte_wiki_validate` | `entityIds`, `paths`, `limit` | Validates the wiki Markdown without needing an index. Diagnostics include a code, severity, remediation and location. |
| `knobyte_wiki_grounding_status` | `id*` | Each grounding of one entity, with its origin (frontmatter or anchor), health and committed baseline. |
| `knobyte_wiki_plan_operation` | `operations` or `operation`, `sessionId` | Dry-runs typed wiki operations and returns the planned diffs plus a single-use plan `handle`, valid for 15 minutes. Nothing is written. |
| `knobyte_wiki_apply_operation` | `handle*`, `sessionId` | Applies a planned handle after re-checking each entity's revision and content hash. Writes the Markdown, appends `events/operations.jsonl` and refreshes the index. **Writes.** |
| `knobyte_read_file` | `file*` | Reads a text file relative to `.knobyte/`. Absolute paths, `..` and symlinks that leave the scaffold are rejected. |

### Drift and health

| Tool | Parameters | What it does |
|---|---|---|
| `knobyte_check` | `fix` | Drift report with score, issues and file count. With `fix: true` it first relocates moved groundings. **Writes with `fix`.** |
| `knobyte_sync_groundings` | `dryRun` | Relocates grounding references whose symbols moved. **Writes unless `dryRun`.** |
| `knobyte_heartbeat` | `staleDays` | Scaffold health: status, stale files with their age, and cleanup status. |

### Team memory

| Tool | Parameters | What it does |
|---|---|---|
| `knobyte_log` | `action` (`read`\|`write`), `kind`, `summary`, `details`, `tags`, `files`, `actor`, `limit` | Reads recent events, or appends a decision, discovery, note, risk or todo as the current actor. `actor` is only a check. **Writes with `write`.** |
| `knobyte_timeline` | `query`, `kind`, `file`, `since`, `includeSuperseded`, `limit` | Searches event history. |
| `knobyte_harvest` | `limit` | Harvests decisions from git history, ADRs and changelogs into the event log, deduplicated. **Writes.** |
| `knobyte_relay_list` | none | Lists published relays. |
| `knobyte_relay_draft` | `title*`, `summary*`, `sender`, `namedRecipients`, `openToTeam`, `progress`, `blockers`, `nextActions`, `evidence` | Saves a local relay draft as the current actor. Recipients must be active members. **Writes a local draft.** |
| `knobyte_inbox_draft` | `reason*`, `title`, `change`, `target`, `content`, `mode`, `evidence`, `author` | Saves a local inbox draft as the current actor. The draft is either a typed `change` (`knowledge.create`, `knowledge.update`, `spec.create`, `spec.update`) or a Markdown edit (`title` + `target` + `content`). **Writes a local draft.** |
| `knobyte_member_list` | none | Registered team members. |
| `knobyte_member_current` | none | The effective local member, or `null`. |
| `knobyte_playbook_list` | `state`, `topic`, `includeArchived`, `cursor`, `limit` | Paged list of team playbooks. Archived playbooks are hidden by default. |
| `knobyte_playbook_get` | `id` or `runId` | One playbook (steps with instructions, required checks and expected evidence, plus recent runs), or one run with its step states and recorded evidence. |
| `knobyte_playbook_complete_step` | `runId*`, `stepId*`, `evidence`, `note` | Completes one pending step of an active run as the current actor, with evidence (`file:<path>`, `commit:<sha>`, `entity:<id>`, `code:<symbol>`, a URL or free text). The run completes with its last step. **Writes.** |
| `knobyte_catch_up` | `since`, `workstream`, `groups`, `includeMine`, `cursor`, `limit` | The current actor's catch-up digest: handoffs addressed to them, proposals awaiting their review, decisions, knowledge, workstream, playbook and other changes since their cursor (or `since`). |
| `knobyte_catch_up_mark` | `at` | Moves the current actor's checkout-local catch-up cursor forward (pass the digest's `observedAt`). Nothing shared is written. **Writes local state.** |

Drafts that agents save are checkout-local. A person publishes them with `knobyte inbox publish`
or `knobyte relay publish`, or from the Hub. Likewise there is no tool that creates, publishes
or archives a playbook, or starts or abandons a run: agents only record step evidence on runs a
person started. See [Team workflows](team-memory-workflows.md).
