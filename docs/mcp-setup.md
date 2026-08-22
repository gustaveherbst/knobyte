# MCP Server

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

Knobyte ships a Model Context Protocol (MCP) server in the same binary. It runs over **stdio**
or over HTTP, where one server offers **Streamable HTTP** (`/mcp`), the older **HTTP + SSE**
transport (`/sse` + `/messages`), or both (the default). Agents get the code graph, vector search,
wiki, drift checks and team memory as 30 tools, grouped into [profiles](#tool-profiles) so a
client loads only the 15-tool `core` set unless you ask for more. The server always works on the
project it was started in.

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
[mcp] Using tool profile core (15 tools, from default)
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
| `GET /health` | Liveness check with the active tool profile: `{"service":"knobyte-mcp","status":"ok","version":"0.9.5","profile":"core","tools":["knobyte_session_start", ...]}`. |

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

The instructions end with the active tool profile, its tool count and the other profiles. All
six tools named above are in every profile.

---

## Tool profiles

Every tool definition an agent loads costs context, and some clients (Cursor, for example) limit
how many tools may be enabled across all servers. Knobyte therefore lists only one **profile** of
its 30 tools. The default, `core`, is the set an agent needs day to day; the other profiles add
one area on top of it, and `full` lists everything.

```bash
knobyte mcp --stdio                    # core (default)
knobyte mcp --stdio --profile team     # core + relays and playbooks
knobyte mcp --stdio --profile wiki     # core + wiki authoring and validation
knobyte mcp --stdio --profile graph    # core + Datalog, PageRank, shortest path, raw graph access
knobyte mcp --stdio --profile full     # all 30 tools
```

The profile is chosen by precedence: the `--profile` flag, then the `KNOBYTE_MCP_PROFILE`
environment variable, then `mcp.profile` in `.knobyte/config.json`, else `core`:

```json
{ "mcp": { "profile": "team" } }
```

An unknown name stops the server with an error naming where it came from. The active profile
is reported at startup (`[knobyte mcp] tool profile core (15 tools, from default)` on stderr
for stdio), in the `initialize` answer (`serverInfo.profile` and the instructions), on
`GET /health` and `GET /`, and on the Hub's MCP page.

| Profile | Tools | Definition size (`tools/list`) | Adds |
|---|---|---|---|
| `core` (default) | 15 | about 12.4k characters, about 3.1k tokens | the day-to-day set below |
| `team` | 18 | about 14.0k characters, about 3.5k tokens | `knobyte_relay_list`, `knobyte_playbooks`, `knobyte_playbook_complete_step` |
| `wiki` | 19 | about 15.2k characters, about 3.8k tokens | `knobyte_wiki_neighborhood`, `knobyte_wiki_validate`, `knobyte_wiki_plan_operation`, `knobyte_wiki_apply_operation` |
| `graph` | 20 | about 14.7k characters, about 3.7k tokens | `knobyte_graph_get`, `knobyte_graph_status`, `knobyte_cozo_datalog`, `knobyte_cozo_pagerank`, `knobyte_cozo_shortest_path` |
| `full` | 30 | about 20.2k characters, about 5.1k tokens | the above plus `knobyte_heartbeat`, `knobyte_read_file`, `knobyte_harvest` |

`core` contains `knobyte_session_start`, `knobyte_graph_scope`, `knobyte_graph_query`,
`knobyte_vector_search`, `knobyte_wiki_search`, `knobyte_wiki_get`, `knobyte_file_context`,
`knobyte_check`, `knobyte_log`, `knobyte_timeline`, `knobyte_catch_up`, `knobyte_members`,
`knobyte_workstream_step_update`, `knobyte_relay_draft` and `knobyte_inbox_draft`. (Sizes are the
sum of the JSON-encoded tool definitions; tokens are estimated at four characters each.)

A call to a tool outside the active profile is refused with an error that names the profiles
which include it, for example:

```text
Tool 'knobyte_cozo_datalog' is not in the active MCP profile 'core'. It is available in
profile(s): graph, full. Restart the server with `knobyte mcp --profile graph` ...
```

Client configuration with a profile (stdio):

```json
{
  "mcpServers": {
    "knobyte": { "command": "knobyte", "args": ["mcp", "--stdio", "--profile", "team"] }
  }
}
```

```bash
claude mcp add knobyte -- knobyte mcp --stdio --profile wiki
```

For an HTTP server, pass the flag when starting it (`knobyte mcp --http --profile graph`); every
client connected to that server sees the same profile.

---

## Tools (30)

Parameters marked `*` are required. Every tool is read-only unless its description says it writes.
The Profiles column lists the profiles that include the tool besides `full`.

### Session and workstreams

| Tool | Profiles | Parameters | What it does |
|---|---|---|---|
| `knobyte_session_start` | core, team, wiki, graph | none | Entry point for a new or resuming agent. Returns project info, the current member, workstreams and the in-progress step, git HEAD and dirty files, the latest relay, recent decisions and risks, and the scaffold heartbeat. |
| `knobyte_workstream_step_update` | core, team, wiki, graph | `status*`, `workstreamId`, `stepId`, `stepIndex`, `evidence`, `filesTouched` | Sets a workstream step's status, recorded as activity of the current actor and stamped with git HEAD and dirty files. It creates a missing step but never a workstream, and refuses archived or done workstreams. **Writes.** |
| `knobyte_file_context` | core, team, wiki, graph | `filePath*` | Recorded events that reference a file, plus the code graph symbols defined in it. |

### Code graph

`graph_query`, `graph_scope` and `graph_get` also accept the protocol v3 budget parameters
`detail`, `max_nodes`, `max_output_tokens`, `max_source_lines` and `fingerprint`;
`graph_scope` also takes `max_files` and `max_flow_steps`.

| Tool | Profiles | Parameters | What it does |
|---|---|---|---|
| `knobyte_graph_query` | core, team, wiki, graph | `relation*`, `target*` | Structural lookup: where a symbol is defined, who calls it, what it calls, or who imports it. Includes index freshness. |
| `knobyte_graph_scope` | core, team, wiki, graph | `task*`, `wiki`, `hybrid` | The symbols, definitions and code neighbourhood most relevant to a natural-language task. `wiki` attaches grounded wiki entities and `hybrid` re-ranks with vector similarity. |
| `knobyte_graph_get` | graph | `ids*` | Source definitions and metadata for node ids or readable refs. |
| `knobyte_graph_status` | graph | none | Node and edge counts, last indexed time and working-tree freshness. |

### Vectors and Datalog (CozoDB)

| Tool | Profiles | Parameters | What it does |
|---|---|---|---|
| `knobyte_vector_search` | core, team, wiki, graph | `query*`, `target` (`code`\|`wiki`), `k`, `minScore` | HNSW similarity search using the project's local embedding backend. Returns `k` matches whenever `k` candidates clear the relevance floor (`minScore`, default 0.2); `belowFloor` counts the nearest candidates the floor left out. Nothing leaves the machine. |
| `knobyte_cozo_datalog` | graph | `script*`, `params` | Read-only CozoScript query over `code_nodes`, `code_edges`, `wiki_entities` and `embedding_meta`. Mutations are rejected. The relation schemas, vector indices and examples are served as the MCP resource `knobyte://reference/datalog-schema`. |
| `knobyte_cozo_pagerank` | graph | `theta`, `iterations` | PageRank centrality over code dependency edges. |
| `knobyte_cozo_shortest_path` | graph | `start*`, `target*` | Shortest dependency path between two symbols, given as node ids, names or readable refs. |

### Wiki

| Tool | Profiles | Parameters | What it does |
|---|---|---|---|
| `knobyte_wiki_search` | core, team, wiki, graph | `query`, `type`, `status`, `topic`, `includeArchived`, `includeBody`, `limit`, `maxTokens`, `cursor` | Ranked, cursor-paged search over one index snapshot; an empty or omitted `query` lists entities (archived ones hidden by default). `includeBody` attaches each entity's Markdown body. A cursor is refused once the wiki changes (`REVISION_CONFLICT`). |
| `knobyte_wiki_get` | core, team, wiki, graph | `id*`, `includeBody`, `limit`, `relationsOffset`, `backlinksOffset` | One entity from a snapshot-bound read: relations, backlinks, source location, index state, and each grounding with its origin (frontmatter or anchor), derived health and committed baseline. The body is opt-in. Answers in the wiki envelope. |
| `knobyte_wiki_neighborhood` | wiki | `id*`, `direction`, `relationTypes`, `depth`, `maxEntities`, `maxTokens`, `includeArchived` | Bounded breadth-first neighbourhood over typed relations. |
| `knobyte_wiki_validate` | wiki | `entityIds`, `paths`, `limit` | Validates the wiki Markdown without needing an index. Diagnostics include a code, severity, remediation and location. |
| `knobyte_wiki_plan_operation` | wiki | `operations` or `operation`, `sessionId` | Dry-runs typed wiki operations and returns the planned diffs plus a single-use plan `handle`, valid for 15 minutes. Nothing is written. |
| `knobyte_wiki_apply_operation` | wiki | `handle*`, `sessionId` | Applies a planned handle after re-checking each entity's revision and content hash. Writes the Markdown, appends `events/operations.jsonl` and refreshes the index. **Writes.** |
| `knobyte_read_file` | full only | `file*` | Reads a text file relative to `.knobyte/`. Absolute paths, `..` and symlinks that leave the scaffold are rejected. (`AGENTS.md`, `ROUTER.md` and `context/stack.md` are also MCP resources.) |

### Drift and health

| Tool | Profiles | Parameters | What it does |
|---|---|---|---|
| `knobyte_check` | core, team, wiki, graph | `fix`, `dryRun` | Drift report with score, issues and file count. With `fix: true` it first relocates moved groundings and returns `{fix, report}`; `dryRun: true` only previews the relocations. **Writes with `fix` unless `dryRun`.** |
| `knobyte_heartbeat` | full only | `staleDays` | Scaffold health: status, stale files with their age, and cleanup status. (`knobyte_session_start` includes the same heartbeat.) |

### Team memory

| Tool | Profiles | Parameters | What it does |
|---|---|---|---|
| `knobyte_log` | core, team, wiki, graph | `action` (`read`\|`write`), `kind`, `summary`, `details`, `tags`, `files`, `actor`, `limit` | Reads recent events, or appends a decision, discovery, note, risk or todo as the current actor. `actor` is only a check. **Writes with `write`.** |
| `knobyte_timeline` | core, team, wiki, graph | `query`, `kind`, `file`, `since`, `includeSuperseded`, `limit` | Searches event history. |
| `knobyte_catch_up` | core, team, wiki, graph | `since`, `workstream`, `groups`, `includeMine`, `cursor`, `limit`, `mark`, `at` | The current actor's catch-up digest: handoffs addressed to them, proposals awaiting their review, decisions, knowledge, workstream, playbook and other changes since their cursor (or `since`). With `mark: true` it instead moves the actor's checkout-local cursor forward (pass the digest's `observedAt` as `at`); nothing shared is written. **Writes local state with `mark`.** |
| `knobyte_members` | core, team, wiki, graph | none | The effective local member (`current`, or `null`) and the registered team members (`members`). |
| `knobyte_relay_draft` | core, team, wiki, graph | `title*`, `summary*`, `sender`, `namedRecipients`, `openToTeam`, `progress`, `blockers`, `nextActions`, `evidence` | Saves a local relay draft as the current actor. Recipients must be active members. **Writes a local draft.** |
| `knobyte_inbox_draft` | core, team, wiki, graph | `reason*`, `title`, `change`, `target`, `content`, `mode`, `evidence`, `author` | Saves a local inbox draft as the current actor. The draft is either a typed `change` (`knowledge.create`, `knowledge.update`, `spec.create`, `spec.update`) or a Markdown edit (`title` + `target` + `content`). **Writes a local draft.** |
| `knobyte_relay_list` | team | none | Lists published relays. |
| `knobyte_playbooks` | team | `id`, `runId`, `state`, `topic`, `includeArchived`, `cursor`, `limit` | Without `id`/`runId`: paged list of team playbooks (archived hidden by default). With `id`: one playbook (steps with instructions, required checks and expected evidence, plus recent runs). With `runId`: one run with its step states and recorded evidence. |
| `knobyte_playbook_complete_step` | team | `runId*`, `stepId*`, `evidence`, `note` | Completes one pending step of an active run as the current actor, with evidence (`file:<path>`, `commit:<sha>`, `entity:<id>`, `code:<symbol>`, a URL or free text). The run completes with its last step. **Writes.** |
| `knobyte_harvest` | full only | `limit` | Harvests decisions from git history, ADRs and changelogs into the event log, deduplicated. **Writes.** |

Drafts that agents save are checkout-local. A person publishes them with `knobyte inbox publish`
or `knobyte relay publish`, or from the Hub. Likewise there is no tool that creates, publishes
or archives a playbook, or starts or abandons a run: agents only record step evidence on runs a
person started. See [Team workflows](team-memory-workflows.md).

### Retired tool names

Earlier releases listed 38 tools. Ten were merged into the tools above. Their old names are no
longer listed, but `tools/call` still accepts them with the same arguments and the same result
shape, so existing prompts and scripts keep working. A retired name is allowed when its merged
tool is in the active profile.

| Retired name | Use instead |
|---|---|
| `knobyte_wiki_show` | `knobyte_wiki_get` with `includeBody: true` |
| `knobyte_wiki_grounding_status` | `knobyte_wiki_get` (the `groundings` field) |
| `knobyte_wiki_list` | `knobyte_wiki_search` without `query` |
| `knobyte_wiki_query` | `knobyte_wiki_search` with `query` (cursor instead of offset paging) |
| `knobyte_member_list` | `knobyte_members` (the `members` field) |
| `knobyte_member_current` | `knobyte_members` (the `current` field) |
| `knobyte_playbook_list` | `knobyte_playbooks` without `id`/`runId` |
| `knobyte_playbook_get` | `knobyte_playbooks` with `id` or `runId` |
| `knobyte_catch_up_mark` | `knobyte_catch_up` with `mark: true` |
| `knobyte_sync_groundings` | `knobyte_check` with `fix: true` (`dryRun: true` to preview) |

### Resources

Besides the tools, the server offers MCP resources: `knobyte://context/stack`,
`knobyte://scaffold/AGENTS`, `knobyte://scaffold/ROUTER`, `knobyte://workstreams/current`,
`knobyte://log/decisions`, `knobyte://reference/datalog-schema` (the CozoScript relations,
indices and examples for `knobyte_cozo_datalog`), and one `knobyte://wiki/{id}` and
`knobyte://relay/{id}` per wiki entity and relay. Three prompts are available as well:
`start-session`, `end-session` and `impact-analysis`.
