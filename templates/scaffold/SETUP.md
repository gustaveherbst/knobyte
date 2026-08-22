# Populating the Knobyte Scaffold

`knobyte setup` creates this scaffold, wires your AI tools to it, indexes the repository and then launches Claude Code or Codex to populate it (after you confirm). When no agent runs, setup still finishes: the files keep their populate marker and the agent instructions ask your first agent session to fill them and run `knobyte setup --finish`. This file is the manual fallback.

## What gets populated

| File | Holds |
|------|-------|
| `AGENTS.md` | Project identity, non-negotiables, commands |
| `ROUTER.md` | Current project state, routing table, behavioural contract |
| `context/architecture.md` | Components and how they connect |
| `context/stack.md` | Technologies, libraries, and why |
| `context/conventions.md` | Naming, structure, patterns, verify checklist |
| `context/decisions.md` | Decision log with reasoning |
| `context/setup.md` | Prerequisites, setup steps, commands, common issues |
| `patterns/` | Task runbooks listed in `patterns/INDEX.md` |

Every file starts with a `knobyte:populate` marker comment. The agent removes it once the file holds real content; setup treats the scaffold as populated when no required file carries it.

## Manual population

1. Run `knobyte setup --print-prompt` for the full population prompt, or `knobyte init` for a pre-analysed brief of the repository (`knobyte init --json`).
2. Give your agent the brief and ask it to fill every annotated section, replacing the annotation comments with real content from this codebase. Unknowns are written as "[TO DETERMINE]" with what is needed to resolve them.
3. Ask it to write 3-5 starter patterns (see `patterns/README.md`) and list them in `patterns/INDEX.md`.
4. Ask it to add `related_to` `relations` between related files and tight `grounds_to` entries for specific behavioural claims, using `knobyte graph scope` output.
5. Run `knobyte setup --finish` to re-scan, capture grounding baselines and rebuild the wiki and vector indexes, then run `knobyte check`.

## After setup

- `knobyte check` reports the drift score.
- `knobyte sync` repairs drift with an agent or prints targeted repair prompts.
- `knobyte watch` installs a post-commit drift check.
