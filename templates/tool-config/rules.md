<!-- knobyte-tool-config: managed copy written by knobyte setup; keep this line so `knobyte check` can detect out-of-sync copies -->

# Project Context (Knobyte)

This repository keeps its project memory in the Knobyte scaffold.

- At the start of every session, read `.knobyte/AGENTS.md` and `.knobyte/ROUTER.md` before project work; follow the routing table in `ROUTER.md` to load only the relevant context.
- Treat the scaffold as the source of truth for architecture, stack, conventions, and decisions; prefer it over re-deriving context from the code.
- Use the code graph (`knobyte graph scope "<task>"`, `knobyte graph query`, `knobyte impact`) before reading many source files.
- After meaningful work, follow the GROW step in `.knobyte/ROUTER.md` and bump `last_updated` on scaffold files you change.
- Do not claim an author, date, or historical event unless the retrieved data actually provides it.
