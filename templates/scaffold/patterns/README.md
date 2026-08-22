# Patterns

This folder holds task-specific guidance: what you would tell your agent if you were sitting next to it. Not generic instructions; project-specific accumulated knowledge.

## How patterns get created

**During setup:** after the `context/` files are populated, the agent writes starter patterns for this project's actual stack, architecture, and conventions.

**Over time:** you or your agent add patterns as they emerge from real work: when something breaks, when a task has a non-obvious gotcha, when the same thing has been explained twice. `knobyte pattern add <name>` creates a pattern file from the template below and lists it in `INDEX.md`.

## What belongs here

A pattern is worth writing when:
- A task type is common in this project and has a repeatable workflow
- Components integrate with gotchas that are not obvious from the code
- Something broke and you want to prevent the same failure
- A verify checklist specific to one task type would catch mistakes early

Skip a pattern only when `context/conventions.md` already covers the same guidance with concrete examples, or the task has no project-specific gotchas.

## Format

Single-task pattern (one file, one task):

```markdown
---
name: <pattern-name>
description: <one line: what this pattern covers and when to use it>
triggers:
  - "<keyword that should load this file>"
relations:
  - type: related_to
    target_id: kb_conventions
    note: when verifying this task
grounds_to: []
last_updated: YYYY-MM-DD
---

# <Pattern Name>

## Context
What to load or know before starting this task type.

## Steps
The workflow, in order.

## Gotchas
What goes wrong and what to watch out for.

## Verify
Checklist to run after completing this task type.

## Debug
What to check when this task type breaks.

## Update Scaffold
- [ ] Update "Current Project State" in ROUTER.md if what works or is not built changed
- [ ] Update any context file that is now out of date
- [ ] New recurring task type without a pattern? Create one and add it to INDEX.md
```

Multi-section pattern: when tasks share context but differ in steps, keep one shared `## Context` and give each task its own `## Task: <name>` heading with Steps, Gotchas and Verify sub-sections. Do not combine unrelated tasks into one file.

## Grounding

Read broad, ground tight. Use `knobyte graph scope "<task>"` to read the neighbourhood, then list in `grounds_to` only the few symbols that embody the documented behaviour, as `kind:path:qualified_name` references copied from graph output. Inline anchors use `<!-- kb-ground: kind:path:qualified_name -->`. Run `knobyte graph ground` after adding groundings so `knobyte check` can detect drift.

## Pattern categories

1. **Common tasks**: what a developer does most often (add an endpoint, add a command, add a component).
2. **Integrations**: external dependencies with non-obvious setup, failure modes or rate limits.
3. **Debug and diagnosis**: one pattern per major failure boundary in the architecture flow.
4. **Deploy and release**: only when `context/setup.md` shows non-trivial deployment.

Do not cap patterns at a number; cap them at whether they add real value.
