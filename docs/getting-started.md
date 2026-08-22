# Getting Started

![Setup, indexes and the Project Hub.](diagrams/readme/setup.svg)

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

This guide builds Knobyte, sets it up in a repository, populates the project memory, and runs a
first drift check. Every output shown here comes from a real run on a three-file Rust project
(`src/main.rs`, `src/auth.rs`, `src/password.rs`). Absolute paths and hashes are shortened.

---

## 1. Build

Knobyte is a single Rust binary. Building it needs **Rust 1.90+**, `cargo` and a C compiler,
because SQLite, the Tree-sitter grammars and `ring` bundle C code. Nothing else is needed at
runtime: no Node.js, no Python, no database server.

```bash
git clone https://github.com/knobyte-ai/knobyte.git
cd knobyte
cargo build --release          # binary: target/release/knobyte
cargo install --path .         # optional: install into ~/.cargo/bin
knobyte --version
```

There is no npm package. Install from source or copy the binary onto your `PATH`.

---

## 2. Set up a repository

Run setup from the repository root. `--dry-run` lists everything it would create without writing:

```bash
knobyte setup --dry-run
knobyte setup --tools claude,codex
```

`--tools` chooses which agent integrations to write. The choices are `claude`, `codex`, `cursor`,
`windsurf`, `copilot` and `opencode`, comma separated, or `none`. Without `--tools`, setup uses
the tools saved in `.knobyte/config.json`, then asks (with `--cli`), and otherwise defaults to
Claude Code. [Agent integration](agent-integration.md) lists the files each tool gets.

```text
[info] Detected: existing codebase with source files; populate the scaffold from code

Creating the .knobyte/ scaffold...
[ok] Created .knobyte/config.json (project configuration (mode: code-repo))
[ok] Created .knobyte/.gitignore (ignore derived databases and checkout-local state)
[ok] Created .knobyte/AGENTS.md (always-loaded project anchor)
[ok] Created .knobyte/ROUTER.md (session bootstrap and routing table)
[ok] Created .knobyte/SETUP.md (manual population guide)
[ok] Created .knobyte/SYNC.md (drift repair guide)
[ok] Created .knobyte/context/architecture.md (architecture overview (kb_architecture))
[ok] Created .knobyte/context/stack.md (technology stack (kb_stack))
[ok] Created .knobyte/context/conventions.md (coding conventions (kb_conventions))
[ok] Created .knobyte/context/decisions.md (decision log (kb_decisions))
[ok] Created .knobyte/context/setup.md (development setup (kb_setup))
[ok] Created .knobyte/patterns/README.md (pattern format guide)
[ok] Created .knobyte/patterns/INDEX.md (pattern index)

AI tools

Installing Knobyte agent skills...
[ok] Install the knobyte-inbox skill at .claude/skills/knobyte-inbox.
[ok] Install the knobyte-relay skill at .claude/skills/knobyte-relay.
[ok] Create CLAUDE.md with the managed Knobyte instruction block.
[ok] Install the knobyte-inbox skill at .agents/skills/knobyte-inbox.
[ok] Install the knobyte-relay skill at .agents/skills/knobyte-relay.
[ok] Create AGENTS.md with the managed Knobyte instruction block.
[info] Start a new agent session so the new skills and project instructions are loaded.
[info] Scanning codebase...
[ok] Pre-analysis complete; the agent will reason from the brief instead of exploring
[info] Building code graph...
[ok] Code graph ready

Populating the scaffold...
```

Setup also creates the empty team directories (`events/`, `team/members/`, `workstreams/`,
`specs/`, `inbox/`, `relays/`, `topics/`, `local/`). If the repository already has a root
`.gitignore`, setup appends Knobyte's database rules to it.

### Modes

`--mode` picks one of four modes:

- `code-repo` is the default.
- `agent-memory` is a memory workspace with no code graph or codebase scan, and it adds `HEARTBEAT.md`.
- `monorepo` and `docs-only` scaffold like `code-repo` but skip the Git checks and the commit
  checkpoint.

The mode is saved, so later runs reuse it.

---

## 3. Populate the memory

The new context files are templates. Each one starts with a `<!-- knobyte:populate -->` marker.
An agent fills them from the code graph, with readable groundings (`kind:path:qualified_name`)
for the claims that depend on specific code.

You decide whether to launch the agent:

- **Interactive terminal:** setup shows the exact command, the working directory and the
  pre-approved read-only `knobyte` commands. It then asks
  `Launch <tool> to populate the scaffold now? [Y/n]`.
- **`--launch-agent`:** launches without the question. This flag is your explicit consent.
- **`--no-agent`, a non-interactive shell, `CI`, or `KNOBYTE_NO_AGENT_LAUNCH=1`:** setup only
  prints the population prompt for you to paste into your agent:

```text
Almost done. One more step: populate the scaffold.
[info] Paste the prompt below into your AI tool. The agent will read your codebase and fill every scaffold file.

------------------------- COPY BELOW THIS LINE -------------------------

You are going to populate the Knobyte project-memory scaffold for this project.
The scaffold lives in the .knobyte/ directory.
...
------------------------- COPY ABOVE THIS LINE -------------------------

[info] Setup paused at population. After the agent finishes, rerun `knobyte setup` to capture groundings and build the wiki index.
```

When the agent has finished and the markers are gone, run `knobyte setup` again. It keeps
everything that was authored and finishes setup (a checkout-local marker,
`.knobyte/local/setup-pending`, tells it that this run completes a paused setup, so it
captures the grounding baselines):

```text
[info] Detected: existing codebase with a populated scaffold; preserve authored files and finish setup
...
Finishing setup from the existing populated scaffold...
Finalizing...
[ok] Captured 3 grounding baseline(s)
[ok] Wiki ready with 11 indexed entities
Commit checkpoint
[info] Review the scoped files, then commit them:
[info] Knobyte did not stage or commit anything.
```

Capturing baselines writes each grounding's `body_hash` and `fingerprint` into the Markdown, so
the baselines are committed with the documentation:

```yaml
grounds_to:
  - ref: function:src/auth.rs:validate_token
    body_hash: a7813b11c6d6…
    fingerprint: mh1:20:05b1fbf5…
```

```markdown
<!-- kb-ground: function:src/password.rs:verify_password #94689e58f48a… -->
```

Setup never commits unless you pass `--commit` or confirm the prompt, and it never pushes.

---

## 4. Check health

```text
$ knobyte check
Drift score: 100/100 — 0 errors, 0 warnings, 0 info
12 files checked
Groundings: 100.0% intact (3 intact, 0 changed, 0 moved, 0 ambiguous, 0 gone, 0 unverified of 3)
graph fresh
[ok] All scaffold files pristine and in sync with codebase.
```

```text
$ knobyte doctor
knobyte doctor
Scaffold: …/demo/.knobyte

ok Drift       100/100 (0 errors, 0 warnings)
ok Graph       fresh; 3 files, 12 nodes, 19 edges
ok Coverage    all recognized source files are indexable
ok Heartbeat   HEARTBEAT_OK
ok Events      0 logged events
ok Wiki        ready (11 entities)
ok CozoDB      not initialized
ok Embeddings  hashed (128-dim)
ok Config      config.json loaded (mode: code-repo, git: yes)
```

Commit `.knobyte/`, the agent files and `CLAUDE.md` / `AGENTS.md`. The databases and `local/`
are already git-ignored.

---

## 5. Register yourself

```bash
knobyte member add alex --name "Alex Rivera" --role engineer --select
```

Name and email default to your `git config`. Knobyte never creates members implicitly. When no
member is selected, the actor is the registered member whose email or Git alias matches
`git config user.email`.

---

## 6. Explore

```bash
knobyte graph query who-calls validate_token      # method Session::is_valid (src/auth.rs:8:4)
knobyte graph scope "session token validation"    # ranked files, source and flows for a task
knobyte impact validate_token                     # transitive blast radius
knobyte wiki query token                          # ranked wiki search
knobyte cozo sync && knobyte cozo search "token validation" --k 3
knobyte hub                                       # the Project Hub in your browser
```

---

## 7. When the code changes

After an edit to `validate_token` and a move of `verify_password` into `src/crypto.rs`:

```text
$ knobyte graph refresh
[ok] Refreshed code graph (incremental) in 3ms: 1 added, 2 modified, 1 deleted; 3 files re-extracted, 0 reused from cache. Totals: 3 files, 9 symbols, 19 relationships. Status: fresh.

$ knobyte check
WARNING

.knobyte/context/architecture.md
  ! GROUNDING_DRIFT Grounded node body changed: function:src/auth.rs:validate_token (src/auth.rs:13). Review the documentation, then run `knobyte graph ground --rebaseline` to accept the current code.
  ! GROUNDING_DRIFT:36 Inline anchor should move: function:src/password.rs:verify_password; candidate: function:src/crypto.rs:verify_password (Exact AST body hash match for function symbol, confidence 100%). Run `knobyte sync` (or `knobyte check --fix`) to rewrite it.

Drift score: 94/100 — 0 errors, 2 warnings, 0 info
12 files checked
Groundings: 33.3% intact (1 intact, 1 changed, 1 moved, 0 ambiguous, 0 gone, 0 unverified of 3)
```

- `knobyte sync` (or `knobyte check --fix`) rewrites the moved reference.
- Once you have reviewed the prose against the new body, `knobyte graph ground --rebaseline`
  accepts the changed code.

[Grounding and drift](grounding-and-drift.md) covers the full workflow.

---

## Joining a repository that already uses Knobyte

```bash
git pull
knobyte setup                 # build this checkout's code graph and wiki index
knobyte member current        # Current member: Sam Lee (sam) via git-alias
knobyte member select <id>    # or: knobyte member add <id> --select
```

The canonical files arrive through Git. Each checkout builds its own `graph.db`, `wiki.db` and
`cozo.db`. On a scaffold that is already populated, `knobyte setup` does not modify tracked
files: it creates missing directories, rebuilds the code graph and the wiki index, and reports
instead of writing.

```text
[info] The scaffold is already populated: tracked files stay as they are; setup refreshes local state (graph, wiki index)
...
Finalizing...
[info] 2 groundings have no committed baseline — run `knobyte graph ground --rebaseline` to capture them
[ok] Wiki index rebuilt with 11 entities; tracked scaffold files were not modified
```

- Scaffold files that `knobyte update` would refresh are listed, not written.
- Agent files (instruction blocks, skills, `aiTools`) are written only when you pass `--tools`;
  otherwise setup reports what is missing.
- Grounding baselines are captured only with `--capture-baselines` (or
  `knobyte graph ground --rebaseline`), which leaves Markdown changes for you to review and
  commit.
- There is no commit checkpoint on a re-run.

---

## Next

- [Agent integration](agent-integration.md): what setup writes for each agent.
- [Team workflows](team-memory-workflows.md): Inbox, Relays, Members and Workstreams.
- [CLI reference](cli-reference.md): every command and flag.
