# Knobyte Documentation

Welcome to the official documentation for **Knobyte** — the 100% Rust persistent project memory, neuro-symbolic code graph, and vector intelligence engine for AI coding agents and engineering teams.

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

---

## Documentation Index

| Guide | Description |
|---|---|
| **[Getting Started](getting-started.md)** | Prerequisites, compiling from source, initial repository setup, and first run |
| **[MCP Server Setup](mcp-setup.md)** | Configuring the Remote SSE & Stdio Model Context Protocol (MCP) server for Cursor, Claude Desktop, Cline, and Windsurf |
| **[CLI Reference Manual](cli-reference.md)** | Complete command-line reference for all 20+ `knobyte` commands and subcommands |
| **[Code Graph & Vector Engine](code-graph-and-vector.md)** | Tree-sitter AST graph indexing, CozoDB Datalog queries, and 128-dim dense HNSW vector search |
| **[Team Memory Workflows](team-memory-workflows.md)** | Context handoffs (Relays), knowledge contribution proposals (Inbox), team members, and audit logs |
| **[AI Agent Integration](agent-integration.md)** | Prompting strategies, router discovery, and MCP tool orchestration for autonomous agents |

---

## What is Knobyte?

AI coding agents are only as good as the context they receive. Without persistent repository memory, agents:
- Re-read hundreds of files redundantly on every turn.
- Violate unwritten team architectural patterns and conventions.
- Cause documentation and code to silently drift apart.
- Drop context between sessions and handoffs.

**Knobyte** solves this natively in pure Rust with:
1. **Deterministic Code Graph**: Tree-sitter powered AST indexing (`graph.db`) providing instant symbol discovery, call graphs, import graphs, and task scope neighborhoods.
2. **Hybrid CozoDB Datalog & HNSW Vector Engine**: Embedded neuro-symbolic storage (`cozo.db`) combining 128-dimensional vector similarity search with relational Datalog graph queries and PageRank algorithms.
3. **Full-Text Wiki Index**: SQLite FTS5 search (`wiki.db`) indexing markdown documentation, YAML frontmatter, relations, and code groundings.
4. **Zero-AI Drift Detection**: Compares code symbol bodies against historical groundings to alert when documentation has drifted from the implementation.
5. **Human & Team Workflows**: Schema v4 Relays (session handoffs), Inbox proposals, and durable event timelines.
6. **Native MCP Server**: First-class support for both remote Server-Sent Events (SSE) and Stdio transports.
