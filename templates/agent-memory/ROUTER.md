---
id: kb_router
name: router
description: Session bootstrap and navigation hub for a persistent AI agent workspace.
type: guide
status: promoted
relations:
  - type: related_to
    target_id: kb_architecture
    note: when working on services, infrastructure, automations, or system shape
  - type: related_to
    target_id: kb_stack
    note: when checking tools, models, runtimes, versions, or hardware
  - type: related_to
    target_id: kb_conventions
    note: when operating on the system or applying safety rules
  - type: related_to
    target_id: kb_decisions
    note: when asking why something is configured a certain way
  - type: related_to
    target_id: kb_setup
    note: when debugging, restarting, recovering, or inspecting services
  - type: related_to
    target_id: kb_heartbeat
    note: when handling a scheduled heartbeat
last_updated: {{TODAY}}
---

<!-- knobyte:populate -->
# Session Bootstrap

Read `AGENTS.md` first if it is not already loaded. Then read this file.

## Current Operational State
<!-- Active systems, known issues, current projects, and anything the agent must know before acting. -->

## Routing Table

| Task type | Load |
|-----------|------|
| System architecture or service topology | `context/architecture.md` |
| Models, tools, hardware, versions, storage | `context/stack.md` |
| Operational rules, naming, safety habits | `context/conventions.md` |
| Why a decision was made | `context/decisions.md` |
| Run, inspect, restart, recover | `context/setup.md` |
| Scheduled heartbeat | `HEARTBEAT.md` |
| Recurring task | `patterns/INDEX.md` |

## Behavioural Contract

1. **CONTEXT**: load only the files relevant to the task.
2. **ACT**: do the requested work using the current operational state.
3. **VERIFY**: check the real system state before claiming success.
4. **DEBUG**: when reality disagrees with the scaffold, trust reality and repair the scaffold.
5. **GROW**: Ground, Record, Orient, Write (bump `last_updated` on every changed file).
