---
id: kb_router
name: router
description: Session bootstrap and navigation hub. Read at the start of every session before any task. Project state, routing table, and behavioural contract.
type: guide
status: promoted
relations:
  - type: related_to
    target_id: kb_architecture
    note: when working on system design, integrations, or how components connect
  - type: related_to
    target_id: kb_stack
    note: when working with specific technologies, libraries, or making tech decisions
  - type: related_to
    target_id: kb_conventions
    note: when writing new code, reviewing code, or unsure about project patterns
  - type: related_to
    target_id: kb_decisions
    note: when making architectural choices or understanding why something is built a certain way
  - type: related_to
    target_id: kb_setup
    note: when setting up the dev environment or running the project for the first time
last_updated: {{TODAY}}
---

<!-- knobyte:populate -->
# Session Bootstrap

If you have not already read `AGENTS.md`, read it now: it holds the project identity, non-negotiables, and commands.

Then read this file fully before doing anything else in this session.

## Current Project State
<!-- What is working, what is not yet built, known issues. Update whenever significant work
     completes: this re-grounds the agent every session.
     Three lists (Working / Not yet built / Known issues), 3-7 items each. -->

## Routing Table

Load the relevant file for the current task. Load `context/architecture.md` first if it is not already in context this session.

| Task type | Load |
|-----------|------|
| Understanding how the system works | `context/architecture.md` |
| Working with a specific technology | `context/stack.md` |
| Writing or reviewing code | `context/conventions.md` |
| Making a design decision | `context/decisions.md` |
| Setting up or running the project | `context/setup.md` |
| Any specific task | Check `patterns/INDEX.md` for a matching pattern |

## Behavioural Contract

For every task, follow this loop:

1. **CONTEXT**: load the relevant context files from the routing table. Check `patterns/INDEX.md` for a matching pattern and follow it when one exists.
2. **BUILD**: do the work. Before deviating from an established pattern, say so and why.
3. **VERIFY**: load `context/conventions.md` and run its Verify Checklist item by item, stating each result.
4. **DEBUG**: when verification fails, check `patterns/INDEX.md` for a debug pattern, fix the issue, and re-run VERIFY.
5. **GROW**: after meaningful work:
   - **Ground**: what changed in reality? Name the changed behaviour, system, command, dependency, or workflow.
   - **Record**: update "Current Project State" above and the affected `context/` files surgically.
   - **Orient**: when the task can recur and no pattern exists, create one (`knobyte pattern add <name>`) and list it in `patterns/INDEX.md`.
   - **Write**: bump `last_updated` in every scaffold file you changed. Read `knobyte logging --json` before optional `knobyte log` notes.
