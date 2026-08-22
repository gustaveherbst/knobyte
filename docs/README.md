# Knobyte Documentation

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

Knobyte is shared project memory, a deterministic code graph and local vector search for
engineers and their coding agents. It is written in Rust and ships as a single binary.

| Guide | What it covers |
|---|---|
| **[Getting started](getting-started.md)** | Build, `setup`, populate, first drift check, joining a repository (with real output) |
| **[Agent integration](agent-integration.md)** | `setup --tools` for Claude Code, Codex, Cursor, Windsurf, Copilot and OpenCode; managed blocks; skills; agent-launch consent; `init`, `update`, `watch`, `logging`, `doctor`, `heartbeat`, `tui`, `completion` |
| **[Grounding and drift](grounding-and-drift.md)** | `kind:path:qualified_name` references, committed baselines, `check` scoring and every checker code, `sync` repair, `graph ground` |
| **[Wiki](wiki.md)** | Entity model, `<!-- kb:entity -->` markers, `wiki apply` operations and audit log, validation codes, migrate, generated views, synthesis, JSON envelope, index maintenance, export |
| **[Code graph and vectors](code-graph-and-vector.md)** | Tree-sitter extraction, TypeScript compiler mode, graph maintenance, queries and protocol v3, CozoDB, embeddings, PageRank, shortest path, Datalog |
| **[Team workflows](team-memory-workflows.md)** | Actors, preview/apply envelopes, exit codes, Members, Inbox, Relays, Workstreams, Specs, Activity, contracts |
| **[Project Hub](hub.md)** | Sign-in, security, every page, jobs, setup wizard, Fleet |
| **[MCP server](mcp-setup.md)** | Transports, security, client configuration, tool profiles, all 30 tools |
| **[CLI reference](cli-reference.md)** | Every command and flag, generated from `--help`; exit codes; environment variables |
| **[Releasing](releasing.md)** | Cutting a release, platform downloads, the macOS quarantine note, optional local signing |

## Layout of a Knobyte project

```text
.knobyte/
  config.json  .gitignore  AGENTS.md  ROUTER.md  SETUP.md  SYNC.md
  context/        architecture, stack, conventions, decisions, setup, and approved knowledge
  patterns/       task patterns + INDEX.md
  specs/  topics/ requirement specs and topic hubs
  team/members/  workstreams/  inbox/  relays/
  events/         decisions.jsonl (event log), activity/, operations.jsonl (wiki audit)
  graph.db  wiki.db  cozo.db      local, rebuildable, git-ignored
  local/          drafts, current member, signing key, journals (git-ignored)
```

Commit everything except the databases and `local/`. Each checkout rebuilds its own indexes.
