# Model Context Protocol (MCP) Setup Guide

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

Knobyte includes a first-class native **Model Context Protocol (MCP)** server supporting both **Server-Sent Events (SSE)** and **Stdio** transports. This allows AI assistants such as Cursor, Claude Desktop, Windsurf, Cline, and custom agents to access your code graph, vector embeddings, wiki, and team memory natively during reasoning loops.

---

## Transport Modes

### 1. Remote SSE Server (`--sse`)

Ideal for networked development, team environments, or connecting multiple AI agents to a single Knobyte instance:

```bash
knobyte mcp --sse --port 3001 --host 0.0.0.0
```

- **SSE Stream Endpoint**: `http://localhost:3001/sse`
- **JSON-RPC Messages Endpoint**: `http://localhost:3001/messages`
- **Health Check Endpoint**: `http://localhost:3001/health`

### 2. Local Stdio Server (`--stdio`)

Ideal for desktop agent clients running on the same machine (e.g., Cursor, Claude Desktop) that spawn Knobyte as a sub-process:

```bash
knobyte mcp --stdio
```

---

## Client Configuration Examples

### Claude Desktop

Edit your Claude Desktop configuration file:
- **macOS**: `~/Library/Application Support/Claude/claude_desktop_config.json`
- **Windows**: `%APPDATA%\Claude\claude_desktop_config.json`

#### Using Stdio:
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

#### Using Remote SSE:
```json
{
  "mcpServers": {
    "knobyte": {
      "url": "http://localhost:3001/sse"
    }
  }
}
```

---

### Cursor

In Cursor:
1. Open **Cursor Settings** -> **Features** -> **MCP**.
2. Click **+ Add New MCP Server**.
3. Fill in:
   - **Name**: `knobyte`
   - **Type**: `sse` (or `command`)
   - **URL**: `http://localhost:3001/sse` (or Command: `/path/to/knobyte mcp --stdio`)

Alternatively, configure `.cursor/mcp.json` in your repository:
```json
{
  "mcpServers": {
    "knobyte": {
      "url": "http://localhost:3001/sse"
    }
  }
}
```

---

### Cline / Roo Code (VS Code Extension)

In Cline's MCP Settings tab:
```json
{
  "mcpServers": {
    "knobyte": {
      "command": "knobyte",
      "args": ["mcp", "--stdio"]
    }
  }
}
```

---

## Native MCP Tools Reference

Knobyte exposes **21 native tools** to AI agents:

### Code Graph & Navigation

| Tool Name | Parameters | Purpose |
|---|---|---|
| `knobyte_graph_query` | `relation` (string), `target` (string) | Structural queries: `where-defined`, `who-calls`, `who-imports` |
| `knobyte_graph_scope` | `task` (string) | Returns bounded neighborhood of symbols relevant to a natural language task |
| `knobyte_graph_get` | `ids` (array of strings) | Retrieves AST node definition, signature, and body source code |
| `knobyte_graph_status` | none | Returns code graph indexing status, node count, edge count, and schema version |

### CozoDB Neuro-Symbolic & Vector Intelligence

| Tool Name | Parameters | Purpose |
|---|---|---|
| `knobyte_vector_search` | `query` (string), `target` ("code"\|"wiki"), `k` (integer) | 128-dim dense HNSW vector similarity search over code snippets and wiki docs |
| `knobyte_cozo_datalog` | `script` (string), `params` (optional JSON) | Executes arbitrary Datalog queries across relational code and wiki tables |
| `knobyte_cozo_pagerank` | `iterations` (integer), `damping` (number) | Computes PageRank centrality scores across call and import dependency graphs |
| `knobyte_cozo_shortest_path` | `start` (string), `target` (string) | BFS shortest call/import path between two symbols |

### Documentation & Wiki

| Tool Name | Parameters | Purpose |
|---|---|---|
| `knobyte_wiki_query` | `text` (string) | SQLite FTS5 full-text search across all markdown entities and frontmatter |
| `knobyte_wiki_show` | `id` (string) | Retrieves a specific wiki entity document, metadata, and relations |
| `knobyte_wiki_list` | none | Lists all indexed wiki topics, patterns, and architecture context files |
| `knobyte_read_file` | `file` (string) | Reads files safely relative to `.knobyte/` scaffold root |

### Drift & Health

| Tool Name | Parameters | Purpose |
|---|---|---|
| `knobyte_check` | none | Calculates drift score (0-100%) by comparing grounded AST bodies against code |
| `knobyte_heartbeat` | none | Validates environment health and removes stale temporary files |

### Team Memory & Attribution

| Tool Name | Parameters | Purpose |
|---|---|---|
| `knobyte_log` | `message` (string), `kind` (string), `tags` (array), `files` (array), `action` ("append"\|"read") | Appends or searches architectural decisions, notes, discoveries, risks, and todos |
| `knobyte_timeline` | `query`, `kind`, `since`, `limit` | Historical event log and timeline retrieval |
| `knobyte_relay_list` | none | Lists active team handoff relays |
| `knobyte_relay_draft` | `action` ("list"\|"save"\|"publish"), `draft` (optional JSON) | Creates, updates, or publishes a context handoff relay |
| `knobyte_inbox_draft` | `action` ("list"\|"save"\|"publish"), `draft` (optional JSON) | Proposes an update or correction to project memory |
| `knobyte_member_list` | none | Lists canonical team members and Git aliases |
| `knobyte_member_current` | none | Displays currently selected local contributor identity |
