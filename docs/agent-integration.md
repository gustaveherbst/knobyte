# AI Agent Integration Guide

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

This guide explains how to configure AI coding assistants (Cursor, Claude Code, Windsurf, Cline, Codex, Aider) to automatically read and leverage Knobyte memory when developing in your repository.

---

## 1. System Prompt & Instruction Files

Place instructions in your agent's system prompt or tool-specific configuration file (`CLAUDE.md`, `.cursorrules`, or `AGENTS.md`).

### Recommended Agent Instructions

```markdown
# Repository Memory & Architecture Rules (Knobyte)

This project uses **Knobyte** (100% Rust) for persistent project memory, code graphs, and handoffs.

Before planning or executing non-trivial code modifications:
1. **Consult Knowledge Router**: Read `.knobyte/ROUTER.md` and related `.knobyte/context/` files to understand existing architectural constraints.
2. **Query the Code Graph**:
   - Locate definitions: `knobyte graph query where-defined <symbol>`
   - Find callers: `knobyte graph query who-calls <symbol>`
   - Task scoping: `knobyte graph scope "<task summary>"`
3. **Semantic Discovery**:
   - Vector similarity search: `knobyte cozo search "<keywords>"`
   - Full-text wiki search: `knobyte wiki query "<keywords>"`
4. **Record Discoveries**:
   - When introducing an architectural choice, log it:
     `knobyte log "<decision summary>" --kind decision`
5. **Handoffs**:
   - At the conclusion of a coding session, draft a relay if passing work to a human or another agent:
     `knobyte relay draft save`
```

---

## 2. Using Knobyte via Native MCP Tools

When your agent is connected to the Knobyte MCP Server (via SSE or Stdio):
- The agent should call `knobyte_graph_scope` or `knobyte_graph_query` before scanning large directory trees.
- For semantic search, call `knobyte_vector_search` with target `"code"` or `"wiki"`.
- To verify that recent code edits have not broken documentation contracts, call `knobyte_check`.

---

## 3. Tool-Specific Setup

### Claude Code CLI

Knobyte supports native skill syncing for Claude Code:

```bash
knobyte skills sync
```

This creates project skills in `.claude/skills/` enabling:
- `/knobyte-inbox`: Draft knowledge proposals directly in Claude Code.
- `/knobyte-relay`: Transfer and claim session handoffs.

### Cursor

Add to your `.cursorrules` in repository root:
```
Always inspect .knobyte/ROUTER.md before making architectural changes.
Use the MCP tools knobyte_graph_query and knobyte_vector_search to discover symbol locations.
```

### Windsurf

Add to `.windsurfrules`:
```
Check .knobyte/ROUTER.md for repository conventions and architecture rules.
```
