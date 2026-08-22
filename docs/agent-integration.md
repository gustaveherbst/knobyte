# Agent Integration

![An agent follows project instructions, ROUTER.md, and MCP tools to retrieve context and code evidence relevant to the task.](diagrams/readme/context-routing.svg)

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

Knobyte works with coding agents in three ways:

- **Instruction files** point the agent at `.knobyte/AGENTS.md` and `.knobyte/ROUTER.md`.
- **Skills** teach Claude Code and Codex the Inbox and Relay workflows.
- **CLI and MCP** let the agent query the graph, the wiki and team memory.

`knobyte setup` installs the first two, and [MCP](mcp-setup.md) provides the third.

---

## `knobyte setup --tools`

```bash
knobyte setup --tools claude,codex,cursor,windsurf,copilot,opencode
```

The tool list is chosen in this order:

1. `--tools` (comma separated, or `none`);
2. the `aiTools` saved in `.knobyte/config.json`;
3. an interactive menu (`--cli` in a terminal);
4. otherwise Claude Code.

The choice is saved to `aiTools`.

| Tool | Files written |
|---|---|
| `claude` | A managed block in `CLAUDE.md`. Skills in `.claude/skills/knobyte-inbox/` and `.claude/skills/knobyte-relay/`. |
| `codex` | A managed block in `AGENTS.md` (repository root). Skills in `.agents/skills/knobyte-inbox/` and `.agents/skills/knobyte-relay/`. |
| `cursor` | `.cursorrules` |
| `windsurf` | `.windsurfrules` |
| `copilot` | `.github/copilot-instructions.md` |
| `opencode` | `.opencode/opencode.json`, with `"instructions": [".knobyte/AGENTS.md", ".knobyte/ROUTER.md"]` |

```text
$ knobyte setup --tools cursor,windsurf,copilot,opencode
...
AI tools
[ok] Created .cursorrules
[ok] Created .windsurfrules
[ok] Created .github/copilot-instructions.md
[ok] Created .opencode/opencode.json
```

### Your files are not clobbered

- **`CLAUDE.md` / `AGENTS.md`:** Knobyte owns only the block between
  `<!-- knobyte-agent:skills:start -->` and `<!-- knobyte-agent:skills:end -->`. A missing file
  is created. In an existing file the block is appended or replaced, and every other byte is
  kept.
- **`.cursorrules`, `.windsurfrules`, `copilot-instructions.md`:**
  - A missing file gets Knobyte's full rules. The first line marks it as a managed copy, so
    `knobyte check` can detect copies that have drifted apart (`TOOL_CONFIG_DRIFT`).
  - An existing file that already mentions `.knobyte/` is left alone.
  - Any other existing file gets a short pointer block between `<!-- knobyte-anchor:start -->`
    and `<!-- knobyte-anchor:end -->`.
- **`opencode.json`:** `.knobyte/AGENTS.md` is added to an existing `instructions` array.
- **Conflicts:** malformed markers, invalid UTF-8 or an oversized block leave the file untouched,
  and setup prints the line to add yourself.

The managed block for Claude Code looks like this:

```markdown
<!-- knobyte-agent:skills:start -->
## Knobyte agent skills
- At the start of every session, read `.knobyte/AGENTS.md` and `.knobyte/ROUTER.md` before project work; follow `ROUTER.md` to load only the relevant context.
- Read `knobyte logging --json` at session start and before optional logging: ...
- When earlier work may inform the task, search history with `knobyte timeline` ...
- Use `/knobyte-inbox` for explicit contributions to project knowledge and `/knobyte-relay` for durable team handoffs. ...
- Skill activation is not approval for canonical actions.
<!-- knobyte-agent:skills:end -->
```

For Codex the skills are referenced as `$knobyte-inbox` and `$knobyte-relay`.

### Skills

| Skill | Teaches the agent to |
|---|---|
| `knobyte-inbox` | Draft a typed knowledge or spec change (or a correction pinned to `inbox target`'s revision), preview it, save the local draft, and leave publishing and review to a person. |
| `knobyte-relay` | Package completed work, work in progress, decisions, blockers, questions, next actions and evidence as a local relay draft for a person to publish. |

Each skill directory contains `SKILL.md`, `agents/openai.yaml`, `references/cli-workflows.md` and
`.knobyte-managed.json`. That last file records the SHA-256 of every installed file.

Knobyte replaces a skill directory only while its files still match those hashes. A directory you
edited, or one Knobyte did not install, stops the sync with a conflict instead of being
overwritten. `knobyte setup --backup-skills` and `knobyte skills sync --backup` move the
conflicting directory to `.knobyte/local/skill-backups/` and install a fresh copy.

```bash
knobyte skills sync                 # the saved aiTools
knobyte skills sync --tool claude   # or codex, or all
knobyte skills sync --dry-run
```

---

## Launching an agent

Two commands can start a headless agent CLI: `knobyte setup`, to populate the scaffold, and
`knobyte sync`, to repair drift. They follow the same rules:

- **Before launching**, Knobyte prints the exact command, the working directory and the
  pre-approved commands. For Claude Code these are read-only `knobyte` commands only:
  `graph scope|get|query|status`, `impact`, `init`, `logging`, `log`, `timeline` and
  `capabilities`, with file edits accepted. Codex runs with a `workspace-write` sandbox.
- **In an interactive terminal** it asks first.
- **`--launch-agent`** is explicit consent and skips the question. `--agent claude|codex` picks
  the agent.
- **Never launched** with `--no-agent`, in CI (`CI` set), in a non-interactive shell, or with
  `KNOBYTE_NO_AGENT_LAUNCH=1`. Knobyte prints the prompt for you to paste instead.
- **The prompt** is written to a private file under `.knobyte/local/agent-sessions/`, which is
  removed afterwards.
- **Timeouts:** 30 minutes for setup and 15 minutes for each sync session.

`knobyte graph ground --agent` (agent-led retro-grounding) follows the same rules: it shows the
command and allowed tools, then asks before launching. Pass `--launch-agent` to launch without
asking, `--dry-run` to print the prompt instead, and `KNOBYTE_NO_AGENT_LAUNCH=1` prevents the launch.

The Hub's setup wizard never launches an agent without a confirmation dialog.

---

## Keeping the integration current

| Command | Purpose |
|---|---|
| `knobyte init [--json]` | Prints the pre-analysed repository brief (manifests, entry points, folders, tooling) that setup gives the agent. It needs no scaffold. Manifests: `package.json`, `pyproject.toml`, `go.mod`, `Cargo.toml`, `Package.swift` (name, tools version, package dependencies, products, and targets with their source paths; executable targets' `main.swift` or `@main` file and test targets become entry points; tooling `swift build`/`swift test`, plus SwiftLint or swift-format when configured) and, without `Package.swift`, an `.xcodeproj`/`.xcworkspace`. The project name comes from the manifest before the git remote. |
| `knobyte update [--dry-run]` | Refreshes Knobyte-owned files (`SETUP.md`, `SYNC.md`, `patterns/README.md`, after a backup in `.knobyte/local/update-backups/`), creates missing scaffold files, refreshes managed blocks and anchors, and upgrades unmodified skills. Populated content is never modified. |
| `knobyte capabilities --json` | A machine-readable catalogue of commands, their availability in this repository and their exit codes, for agents. |
| `knobyte logging [mode]` | How much agents log unprompted: `significant` (default), `checkpoints` or `manual`. Per checkout. |
| `knobyte watch` | Installs a post-commit drift check, or runs the heartbeat on an interval (`--interval`). |
| `knobyte heartbeat [--clean]` | Reports stale scaffold files (`heartbeat.staleDays`, default 7) and leftover temporary files. `--clean` removes temporary files and stale locks older than an hour. |
| `knobyte doctor` | A one-screen summary of drift, graph, coverage, heartbeat, events, wiki, CozoDB, embeddings and config. Exits 1 on drift errors. |
| `knobyte tui` | A terminal dashboard: drift, graph, heartbeat and events. Keys: `1`-`4`/Tab switch views, `r` refreshes, `l` logs an event, `q` quits. |
| `knobyte completion <bash\|zsh\|fish>` | Prints a shell completion script. |
| `knobyte pattern add <name>` | Creates `patterns/<name>.md` (never overwrites) and adds it to `patterns/INDEX.md`. |

```text
$ knobyte update --dry-run
[ok] Knobyte files are up to date.
Populated content was not modified.

$ knobyte heartbeat
HEARTBEAT_OK
```

---

## What a session looks like

The agent's instructions come from the managed blocks, the skills and the MCP server's operating
rules. A typical session:

1. Read `.knobyte/AGENTS.md` and `.knobyte/ROUTER.md`, or call `knobyte_session_start` over MCP.
2. Explore with `knobyte graph scope "<task>"`, `knobyte graph query who-calls <symbol>`,
   `knobyte impact <symbol>` and `knobyte wiki query "<text>"`.
3. Record decisions with `knobyte log "<summary>" --kind decision`, as `knobyte logging` allows.
4. Propose knowledge changes as Inbox drafts and leave a relay draft at the end.
5. Leave publishing, approval, claiming and pushing to a person.

[Team workflows](team-memory-workflows.md) covers the human side.
