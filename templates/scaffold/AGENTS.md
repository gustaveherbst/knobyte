---
id: kb_agents
name: agents
description: Always-loaded project anchor. Read this first. Project identity, non-negotiables, commands, and the pointer to ROUTER.md for full context.
type: guide
status: promoted
relations:
  - type: related_to
    target_id: kb_router
    note: for full project context, routing, and the behavioural contract
last_updated: {{TODAY}}
---

<!-- knobyte:populate -->
> **Population pending.** This file still holds Knobyte's template. Fill every `.knobyte/` file that carries the populate marker from the code (`knobyte setup --print-prompt` prints the full instructions), delete this note and the marker, then run `knobyte setup --finish`.

# {{PROJECT_NAME}}

## What This Is
<!-- One sentence: what does this project do? A factual description of the software, not a
     tagline. Example: "A REST API for managing inventory across warehouse locations." -->

## Non-Negotiables
<!-- Hard rules the agent must never violate: the things that cause real damage when broken.
     3-5 items. More than 5 means the list has not been prioritised.
     Example:
     - Never write database queries outside the repository layer
     - Never commit secrets or API keys -->

## Commands
<!-- The exact commands needed to work on this project: build, test, lint, run.
     Use the real commands from this codebase, grouped by area for monorepos.
     Keep this whole file under about 200 tokens. -->

## Code Graph
The repository is indexed into `.knobyte/graph.db`. Use it to avoid re-reading code you already have; it is one tool alongside search, not a replacement for it.
- Know the symbol name? Go straight to it: `knobyte graph query where-defined <symbol>`, `knobyte graph query who-calls <symbol>`, `knobyte graph query what-calls <symbol>`, then `knobyte graph get <id...> --source` (pass several ids to one call).
- Exploring an unfamiliar task? `knobyte graph scope "<task>"` returns bounded, source-backed context. It matches words, not meaning: treat it as starting evidence.
- Treat source returned by the graph as already read; do not re-open those files.
- Before editing a symbol, run `knobyte impact <symbol|file>` to see affected callers and the scaffold documents grounded to it.
- During `knobyte sync`, repair drifted groundings and re-check with `knobyte check`.

## Scaffold Growth
After meaningful work, run GROW:
- Ground: what changed in reality?
- Record: update `ROUTER.md` and the relevant `context/` files
- Orient: create or update a `patterns/` runbook when the task can recur (`knobyte pattern add <name>`)
- Write: bump `last_updated` on changed scaffold files; optional `knobyte log` notes follow the logging policy below

## Agent Logging
Read `knobyte logging --json` at session start and before optional logging. The checkout-local advisory mode is `significant` (quiet default: material decisions, risks, blockers, durable discoveries), `checkpoints` (batch useful notes at task or session boundaries), or `manual` (no unsolicited notes). Skip routine tool calls, edits and repeated status. Always honor explicit user log requests.

When earlier work may inform the task, search history with `knobyte timeline` and treat matches as historical notes, not accepted current knowledge.

## Team Memory
Propose knowledge changes as Inbox drafts and hand off work as Relay drafts. Publishing, approving, rejecting and closing are human decisions that need explicit confirmation. Treat Git commit, push and pull as separate actions requiring their own authorization.

## Navigation
At the start of every session, read `ROUTER.md` before doing anything else. It holds the project state, the routing table and the behavioural contract.
