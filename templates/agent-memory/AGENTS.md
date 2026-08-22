---
id: kb_agents
name: agents
description: Always-loaded operating contract for a persistent AI agent workspace.
type: guide
status: promoted
relations:
  - type: related_to
    target_id: kb_router
    note: for full project context, routing, and the behavioural contract
last_updated: {{TODAY}}
---

<!-- knobyte:populate -->
# {{PROJECT_NAME}}

## What This Is
<!-- One sentence: what environment or agent does this scaffold describe? -->

## Non-Negotiables
<!-- 3-5 hard safety or operational rules the agent must never violate. -->

## Commands
<!-- Exact commands for health checks, service status, restart and recovery, and Knobyte
     maintenance (`knobyte heartbeat`, `knobyte check`). -->

## GROW
After meaningful work:
- Ground: what changed in reality?
- Record: update `ROUTER.md` and the relevant `context/` files
- Orient: create or update a `patterns/` runbook when this can recur
- Write: bump `last_updated`; optional `knobyte log` notes follow the logging policy below

## Agent Logging
Read `knobyte logging --json` at session start and before optional logging. The checkout-local advisory mode is `significant` (material decisions, risks, blockers, durable discoveries), `checkpoints` (batch notes at task or session boundaries), or `manual` (no unsolicited notes). Always honor explicit user log requests.

## Heartbeat
When invoked for a heartbeat, read `HEARTBEAT.md`. If every check passes, respond with exactly `HEARTBEAT_OK`.

## Navigation
At the start of every normal session, read `ROUTER.md` before doing anything else.
