# Knobyte CLI Reference

Every `knobyte` command, subcommand and flag. The per-command sections below are generated
from `knobyte <command> --help` for Knobyte 0.9.5, so they match the binary exactly. For task-oriented
explanations, follow the links to the guides.

`knobyte commands` prints a one-page overview, and `knobyte capabilities --json` returns the
same catalogue as machine-readable descriptors with exit codes.

---

## Conventions shared by many commands

### Bare `knobyte`

Running `knobyte` with no subcommand starts the [Project Hub](hub.md) on `127.0.0.1:4000` and
opens the browser (global flags `--port N` and `--no-open`). In a repository without a
`.knobyte/` scaffold the Hub opens on its Setup page. `--port` and `--no-open` are only valid on
bare `knobyte` and `knobyte hub`; anywhere else they are a usage error (exit 2).

### Commands that need a project

Most commands need a `.knobyte/` scaffold above the current directory. Without one they exit 3;
with `--json` they print `{"schemaVersion":1,"ok":false,"error":{"code","reason","message"}}`.
`setup`, `init`, `hub`, `mcp`, `graph`, `impact`, `completion`, `commands`, `capabilities`,
`cozo model pull` and `cozo model status` run without a scaffold. `graph` and `impact` index
any directory (`--root`).

### JSON output

- Team commands (`member`, `activity`, `workstream`, `spec`, `inbox`, `relay`) emit the schema v1
  team envelope with `--json`:
  `{schemaVersion, command, mode: "read"|"preview"|"apply", ok, data, diagnostics, problem}`.
  Lists are bounded pages (`--limit` 1-100, default 50; continue with `--cursor`).
- Wiki commands emit `{schemaVersion, ok, data, diagnostics}` (see [Wiki](wiki.md)).
- `graph query|scope|get` and `impact` accept `--jsonl` (agent protocol v3), and any budget flag
  (`--detail`, `--max-nodes`, `--max-files`, `--max-flow-steps`, `--max-output-tokens`,
  `--max-source-lines`, `--fingerprint`) selects that protocol.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success (including an exact idempotent replay of a team operation) |
| 1 | Validation failed (for `knobyte check`: drift errors were found) |
| 2 | Usage: invalid arguments, request JSON or preview envelope |
| 3 | Unavailable: not found, or no Knobyte project |
| 4 | Conflict: a revision changed since it was read, or an interrupted operation |
| 5 | Refused: unauthorized (for example `ACTOR_MISMATCH`, `SELF_APPROVAL_REQUIRED`) or a path outside the project |
| 130 | Cancelled agent session |

### Team mutation flags

Every team command that changes state accepts the same four flags (see
[Team workflows](team-memory-workflows.md#preview-and-apply)):

- `--preview`: print the signed preview envelope (the exact file changes) without writing anything.
- `--apply <ENVELOPE>`: apply the complete envelope printed by `--preview --json`; refused if
  anything it read has changed, if it is older than 30 minutes, or if it was issued by another
  checkout or for another actor.
- `--request <FILE>`: take the action from a request JSON file (schemas: `knobyte <group> contract`).
- `--operation-id <ID>`: a stable operation id; replaying the exact same operation is idempotent.

Without `--preview` or `--apply` a mutation previews and applies in one step.

### Environment variables

| Variable | Effect |
|---|---|
| `KNOBYTE_HOME` | Per-user directory (default `~/.knobyte`): Hub fleet registry `projects.json` and default model directory |
| `KNOBYTE_MODELS_DIR` | Where `cozo model pull` stores Model2Vec models (default `$KNOBYTE_HOME/models`) |
| `KNOBYTE_NO_AGENT_LAUNCH` | Set to `1` to make `setup` and `sync` never launch an agent CLI |
| `KNOBYTE_HUB_TOKEN` | Access token required by the Hub (same as `hub --token`) |
| `KNOBYTE_MCP_TOKEN` | Bearer token required by the MCP HTTP server (same as `mcp --token`) |
| `KNOBYTE_MCP_PROFILE` | MCP tool profile: `core`, `team`, `wiki`, `graph` or `full` (overridden by `mcp --profile`; overrides `mcp.profile` in `.knobyte/config.json`) |

---


## Command index

| Command | Purpose |
|---|---|
| [`knobyte hub`](#knobyte-hub) | Launch the local Project Hub web interface (or setup if unconfigured) |
| [`knobyte cozo`](#knobyte-cozo) | CozoDB Datalog graph queries and HNSW vector search |
| [`knobyte cozo query`](#knobyte-cozo-query) | Execute a CozoScript Datalog query (read-only unless --mutable) |
| [`knobyte cozo search`](#knobyte-cozo-search) | HNSW vector similarity search on code nodes or wiki entities |
| [`knobyte cozo pagerank`](#knobyte-cozo-pagerank) | Compute PageRank centrality scores on code dependency graph |
| [`knobyte cozo shortest-path`](#knobyte-cozo-shortest-path) | Find shortest path between two code nodes |
| [`knobyte cozo sync`](#knobyte-cozo-sync) | Synchronize SQLite graph.db and wiki.db into CozoDB |
| [`knobyte cozo model`](#knobyte-cozo-model) | Manage the embedding backend and local embedding models |
| [`knobyte cozo model pull`](#knobyte-cozo-model-pull) | Download a Model2Vec model from Hugging Face into ~/.knobyte/models (explicit only) |
| [`knobyte cozo model status`](#knobyte-cozo-model-status) | Show the active embedding backend, model path, dimension and index state |
| [`knobyte cozo model use`](#knobyte-cozo-model-use) | Select the embedding backend (hashed \| model2vec), saved in .knobyte/config.json |
| [`knobyte setup`](#knobyte-setup) | Set up Knobyte project memory: scaffold, AI tool anchors, skills, graph, population |
| [`knobyte init`](#knobyte-init) | Print a pre-analysed brief of the repository (manifests, entry points, folders, tooling) |
| [`knobyte update`](#knobyte-update) | Refresh Knobyte-owned scaffold files and managed blocks without touching populated content |
| [`knobyte check`](#knobyte-check) | Check project memory drift against current codebase |
| [`knobyte sync`](#knobyte-sync) | Repair drift: relocate moved groundings, then fix files with an agent or printed prompts |
| [`knobyte graph`](#knobyte-graph) | Build or query the deterministic code graph (bare `knobyte graph` builds it) |
| [`knobyte graph status`](#knobyte-graph-status) | Read-only health: fresh, stale, degraded, corrupt, rebuild_required or missing |
| [`knobyte graph refresh`](#knobyte-graph-refresh) | Incremental refresh: re-extract only changed files, publish atomically |
| [`knobyte graph rebuild`](#knobyte-graph-rebuild) | Full rebuild into an isolated candidate, published atomically |
| [`knobyte graph query`](#knobyte-graph-query) | Structural lookup: where-defined, who-calls, what-calls, who-imports |
| [`knobyte graph scope`](#knobyte-graph-scope) | Task-scoped retrieval: ranked files, source, directed flows and facts in one response |
| [`knobyte graph get`](#knobyte-graph-get) | Nodes by id or grounding reference, optionally with their source |
| [`knobyte graph ground`](#knobyte-graph-ground) | Grounding baselines and retro-grounding (default: re-baseline grounded references) |
| [`knobyte graph repair`](#knobyte-graph-repair) | Repair the graph store in place (WAL recovery, index/FTS rebuild, schema upgrade, dangling rows) |
| [`knobyte impact`](#knobyte-impact) | Find blast radius / impact of a symbol, file or grounding reference |
| [`knobyte wiki`](#knobyte-wiki) | Build, query, validate, edit (apply) and synthesize the project Wiki |
| [`knobyte wiki list`](#knobyte-wiki-list) | List entities (archived hidden unless --include-archived) |
| [`knobyte wiki show`](#knobyte-wiki-show) | Show one entity with relations, backlinks, groundings and sources |
| [`knobyte wiki query`](#knobyte-wiki-query) | Ranked search: id > title > summary > body |
| [`knobyte wiki related`](#knobyte-wiki-related) | Bounded neighbourhood of an entity |
| [`knobyte wiki backlinks`](#knobyte-wiki-backlinks) | Entities that relate to this one |
| [`knobyte wiki for-code`](#knobyte-wiki-for-code) | Entities grounded in code symbols (graph ids or readable refs) |
| [`knobyte wiki graph`](#knobyte-wiki-graph) | Bounded slice of the entity graph (around the given ids, or the first --limit entities) |
| [`knobyte wiki trace`](#knobyte-wiki-trace) | Spec → requirement → decision → component → code → test traceability |
| [`knobyte wiki validate`](#knobyte-wiki-validate) | Validate the wiki Markdown (no index needed) |
| [`knobyte wiki rebuild-index`](#knobyte-wiki-rebuild-index) | Rebuild the wiki search index (and sync CozoDB) |
| [`knobyte wiki apply`](#knobyte-wiki-apply) | Apply typed wiki operations from a JSON file (object, array, or JSONL) |
| [`knobyte wiki regenerate-views`](#knobyte-wiki-regenerate-views) | Rewrite stale `<!-- kb:generated:begin -->` sections |
| [`knobyte wiki synthesis`](#knobyte-wiki-synthesis) | Agent-driven synthesis of wiki knowledge from the code graph |
| [`knobyte wiki synthesis build`](#knobyte-wiki-synthesis-build) | Discover clusters and produce the agent playbook |
| [`knobyte wiki synthesis prepare`](#knobyte-wiki-synthesis-prepare) | Deterministic context and prompts for one stage |
| [`knobyte wiki synthesis propose`](#knobyte-wiki-synthesis-propose) | Validate an agent response into operation plans (writes only with --apply) |
| [`knobyte wiki migrate`](#knobyte-wiki-migrate) | Rewrite older Knobyte wiki formats (legacy statuses, `document` types, derived ids, old grounding shapes, legacy `edges`) through audited operations. Plans only unless --apply |
| [`knobyte wiki index`](#knobyte-wiki-index) | Index maintenance: state, normalized dump, doctor |
| [`knobyte wiki index status`](#knobyte-wiki-index-status) | The index state (missing, fresh, stale, degraded, rebuild_required, corrupt, migration_required) with its revision |
| [`knobyte wiki index dump`](#knobyte-wiki-index-dump) | Deterministic normalized dump of the index (rows ordered by key, wall-clock excluded) |
| [`knobyte wiki index doctor`](#knobyte-wiki-index-doctor) | Integrity check plus a diff of the index against a clean rebuild of the Markdown |
| [`knobyte export`](#knobyte-export) | Export the whole scaffold as one Markdown bundle (stdout, or --out PATH) |
| [`knobyte member`](#knobyte-member) | Manage team member attribution and identity |
| [`knobyte member contract`](#knobyte-member-contract) | Versioned JSON Schema catalog of member request files |
| [`knobyte member list`](#knobyte-member-list) | List canonical team members |
| [`knobyte member show`](#knobyte-member-show) | Show one member |
| [`knobyte member current`](#knobyte-member-current) | Show the effective actor, how it was resolved, and the local selection |
| [`knobyte member add`](#knobyte-member-add) | Register a new team member (name/email default to git config) |
| [`knobyte member update`](#knobyte-member-update) | Update a member's display name, email, role or Git aliases |
| [`knobyte member deactivate`](#knobyte-member-deactivate) | Mark a member inactive (history is kept) |
| [`knobyte member reactivate`](#knobyte-member-reactivate) | Restore an inactive member |
| [`knobyte member select`](#knobyte-member-select) | Select this checkout's current member |
| [`knobyte member clear`](#knobyte-member-clear) | Clear this checkout's member selection |
| [`knobyte activity`](#knobyte-activity) | View canonical Activity history |
| [`knobyte activity contract`](#knobyte-activity-contract) | Versioned JSON Schema catalog of activity request files |
| [`knobyte activity list`](#knobyte-activity-list) | List canonical activity, newest first |
| [`knobyte activity show`](#knobyte-activity-show) | Show one activity record |
| [`knobyte activity record`](#knobyte-activity-record) | Record a custom activity event |
| [`knobyte activity timeline`](#knobyte-activity-timeline) | Merged timeline of canonical activity and the decision/event log |
| [`knobyte workstream`](#knobyte-workstream) | Manage team workstreams |
| [`knobyte workstream contract`](#knobyte-workstream-contract) | Versioned JSON Schema catalog of workstream request files |
| [`knobyte workstream list`](#knobyte-workstream-list) | List workstreams (archived hidden unless requested) |
| [`knobyte workstream show`](#knobyte-workstream-show) | Show one workstream |
| [`knobyte workstream create`](#knobyte-workstream-create) | Create a workstream |
| [`knobyte workstream update`](#knobyte-workstream-update) | Update a workstream |
| [`knobyte workstream archive`](#knobyte-workstream-archive) | Archive a workstream |
| [`knobyte spec`](#knobyte-spec) | List or view requirements specs |
| [`knobyte spec list`](#knobyte-spec-list) | List specs with lifecycle and grounding health |
| [`knobyte spec show`](#knobyte-spec-show) | Show a spec (id or path such as specs/auth.md) with its requirements, acceptance criteria, constraints and grounding rollup |
| [`knobyte inbox`](#knobyte-inbox) | Propose additions or corrections to project memory |
| [`knobyte inbox target`](#knobyte-inbox-target) | Resolve an existing knowledge record and its exact revision for a correction |
| [`knobyte inbox contract`](#knobyte-inbox-contract) | Versioned JSON Schema catalog of Inbox request files |
| [`knobyte inbox draft`](#knobyte-inbox-draft) | Local inbox drafts (list, show, save, delete) |
| [`knobyte inbox draft list`](#knobyte-inbox-draft-list) | List local drafts |
| [`knobyte inbox draft show`](#knobyte-inbox-draft-show) | Show one complete local draft |
| [`knobyte inbox draft save`](#knobyte-inbox-draft-save) | Save (create or, with --draft-id, replace) a local draft |
| [`knobyte inbox draft delete`](#knobyte-inbox-draft-delete) | Delete a local draft |
| [`knobyte inbox publish`](#knobyte-inbox-publish) | Publish a local draft as a pending proposal |
| [`knobyte inbox proposal`](#knobyte-inbox-proposal) | Published proposals (list, show, approve, reject, withdraw, mark-stale, repair) |
| [`knobyte inbox proposal list`](#knobyte-inbox-proposal-list) | List proposals |
| [`knobyte inbox proposal show`](#knobyte-inbox-proposal-show) | Show one proposal |
| [`knobyte inbox proposal approve`](#knobyte-inbox-proposal-approve) | Approve a pending proposal and apply its knowledge change |
| [`knobyte inbox proposal reject`](#knobyte-inbox-proposal-reject) | Reject a pending proposal |
| [`knobyte inbox proposal withdraw`](#knobyte-inbox-proposal-withdraw) | Withdraw your own pending proposal |
| [`knobyte inbox proposal mark-stale`](#knobyte-inbox-proposal-mark-stale) | Mark a pending proposal stale after its target changed |
| [`knobyte inbox proposal repair`](#knobyte-inbox-proposal-repair) | Repair a stale proposal back to pending with new content |
| [`knobyte inbox approve`](#knobyte-inbox-approve) | Approve a pending proposal (shorthand for `inbox proposal approve`) |
| [`knobyte inbox reject`](#knobyte-inbox-reject) | Reject a pending proposal (shorthand for `inbox proposal reject`) |
| [`knobyte inbox withdraw`](#knobyte-inbox-withdraw) | Withdraw your own pending proposal (shorthand) |
| [`knobyte relay`](#knobyte-relay) | Prepare and exchange context handoffs (relays) |
| [`knobyte relay contract`](#knobyte-relay-contract) | Versioned JSON Schema catalog of Relay request files |
| [`knobyte relay draft`](#knobyte-relay-draft) | Local relay drafts (list, show, save, delete) |
| [`knobyte relay draft list`](#knobyte-relay-draft-list) | List local relay drafts |
| [`knobyte relay draft show`](#knobyte-relay-draft-show) | Show one complete local relay draft |
| [`knobyte relay draft save`](#knobyte-relay-draft-save) | Save (create or, with --draft-id, replace) a local relay draft |
| [`knobyte relay draft delete`](#knobyte-relay-draft-delete) | Delete a local relay draft |
| [`knobyte relay list`](#knobyte-relay-list) | List relays |
| [`knobyte relay publish`](#knobyte-relay-publish) | Publish a local draft (captures branch/HEAD) |
| [`knobyte relay show`](#knobyte-relay-show) | Show one relay |
| [`knobyte relay acknowledge`](#knobyte-relay-acknowledge) | Claim a published relay |
| [`knobyte relay close`](#knobyte-relay-close) | Close an acknowledged relay (sender or claimant) |
| [`knobyte playbook`](#knobyte-playbook) | Reusable team playbooks and their runs |
| [`knobyte playbook contract`](#knobyte-playbook-contract) | Versioned JSON Schema catalog of playbook request files |
| [`knobyte playbook list`](#knobyte-playbook-list) | List playbooks (archived hidden unless requested) |
| [`knobyte playbook show`](#knobyte-playbook-show) | Show a playbook with its steps and recent runs |
| [`knobyte playbook create`](#knobyte-playbook-create) | Create a playbook (draft unless --state active) |
| [`knobyte playbook update`](#knobyte-playbook-update) | Update a playbook; --step replaces the whole step list, --state active publishes a draft |
| [`knobyte playbook archive`](#knobyte-playbook-archive) | Archive a playbook (archived playbooks are immutable and cannot be run) |
| [`knobyte playbook run`](#knobyte-playbook-run) | Start, inspect and advance playbook runs |
| [`knobyte playbook run start`](#knobyte-playbook-run-start) | Start a run of an active playbook (its steps are snapshotted) |
| [`knobyte playbook run list`](#knobyte-playbook-run-list) | List runs, newest first |
| [`knobyte playbook run show`](#knobyte-playbook-run-show) | Show one run with its step states and evidence |
| [`knobyte playbook run complete-step`](#knobyte-playbook-run-complete-step) | Complete one pending step, recording evidence (the run completes with its last step) |
| [`knobyte playbook run abandon`](#knobyte-playbook-run-abandon) | Abandon an active run |
| [`knobyte catch-up`](#knobyte-catch-up) | What changed in shared memory since you last caught up (mark, reset) |
| [`knobyte catch-up mark`](#knobyte-catch-up-mark) | Mark everything up to now (or --at) as seen: advances your checkout-local cursor |
| [`knobyte catch-up reset`](#knobyte-catch-up-reset) | Reset your cursor (also adopts the current branch); --clear removes it |
| [`knobyte catch-up contract`](#knobyte-catch-up-contract) | Versioned JSON Schema catalog of catch-up request files |
| [`knobyte log`](#knobyte-log) | Append a note, decision, discovery, risk, or todo to the event log |
| [`knobyte timeline`](#knobyte-timeline) | Search recent event log entries and project notes |
| [`knobyte heartbeat`](#knobyte-heartbeat) | Health check: stale content docs, workstream consistency, temp-file cleanup |
| [`knobyte doctor`](#knobyte-doctor) | Comprehensive health diagnostic summary |
| [`knobyte skills`](#knobyte-skills) | Sync official agent skills for Claude Code and Codex |
| [`knobyte skills sync`](#knobyte-skills-sync) | Install or update the official skills and managed instruction blocks (never clobbers edits) |
| [`knobyte capabilities`](#knobyte-capabilities) | Structured capability discovery for AI agents |
| [`knobyte mcp`](#knobyte-mcp) | Start the Model Context Protocol (MCP) server (HTTP: streamable /mcp and legacy /sse) |
| [`knobyte pattern`](#knobyte-pattern) | Create a new pattern template |
| [`knobyte pattern add`](#knobyte-pattern-add) | Create patterns/<name>.md (never overwrites) and list it in patterns/INDEX.md |
| [`knobyte logging`](#knobyte-logging) | Read or set this checkout's advisory agent logging mode |
| [`knobyte watch`](#knobyte-watch) | Install/uninstall a post-commit drift check, or run the heartbeat on an interval |
| [`knobyte completion`](#knobyte-completion) | Print a shell completion script (bash, zsh or fish) |
| [`knobyte tui`](#knobyte-tui) | Interactive terminal dashboard |
| [`knobyte commands`](#knobyte-commands) | Print list of all available commands |

## `knobyte hub`

Launch the local Project Hub web interface (or setup if unconfigured)

```text
Usage: knobyte hub [OPTIONS]

Options:
      --port <PORT>    Loopback port to bind (1-65535, default 4000)
      --host <HOST>    [default: 127.0.0.1]
      --no-open        
      --token <TOKEN>  Access token required by the Hub (exported as KNOBYTE_HUB_TOKEN)
  -h, --help           Print help
```

## `knobyte cozo`

CozoDB Datalog graph queries and HNSW vector search

```text
Usage: knobyte cozo <COMMAND>

Commands:
  query          Execute a CozoScript Datalog query (read-only unless --mutable)
  search         HNSW vector similarity search on code nodes or wiki entities
  pagerank       Compute PageRank centrality scores on code dependency graph
  shortest-path  Find shortest path between two code nodes
  sync           Synchronize SQLite graph.db and wiki.db into CozoDB
  model          Manage the embedding backend and local embedding models

Options:
  -h, --help  Print help
```

### `knobyte cozo query`

Execute a CozoScript Datalog query (read-only unless --mutable)

```text
Usage: knobyte cozo query [OPTIONS] <SCRIPT>

Arguments:
  <SCRIPT>  

Options:
      --params <PARAMS>  
      --mutable          Allow the script to modify stored relations (:put, :rm, :create, ...)
      --json             
  -h, --help             Print help
```

### `knobyte cozo search`

HNSW vector similarity search on code nodes or wiki entities

```text
Usage: knobyte cozo search [OPTIONS] <QUERY>

Arguments:
  <QUERY>  

Options:
      --target <TARGET>    What to search: code nodes or wiki entities [default: code] [possible values: code, wiki]
      --k <K>              Number of matches to return (fewer only when fewer candidates clear the floor) [default: 10]
      --min-score <SCORE>  Relevance floor, 0-1 (score = 1 - cosine distance; default 0.20, 0 disables it)
      --json               Print the matches as a JSON array (floor notes go to stderr)
  -h, --help               Print help
```

### `knobyte cozo pagerank`

Compute PageRank centrality scores on code dependency graph

```text
Usage: knobyte cozo pagerank [OPTIONS]

Options:
      --damping <DAMPING>        Damping factor (alias: --theta) [default: 0.85]
      --iterations <ITERATIONS>  [default: 20]
      --json                     
  -h, --help                     Print help
```

### `knobyte cozo shortest-path`

Find shortest path between two code nodes

```text
Usage: knobyte cozo shortest-path [OPTIONS] <START> <TARGET>

Arguments:
  <START>   
  <TARGET>  

Options:
      --json  
  -h, --help  Print help
```

### `knobyte cozo sync`

Synchronize SQLite graph.db and wiki.db into CozoDB

```text
Usage: knobyte cozo sync [OPTIONS]

Options:
      --json  
  -h, --help  Print help
```

### `knobyte cozo model`

Manage the embedding backend and local embedding models

```text
Usage: knobyte cozo model <COMMAND>

Commands:
  pull    Download a Model2Vec model from Hugging Face into ~/.knobyte/models (explicit only)
  status  Show the active embedding backend, model path, dimension and index state
  use     Select the embedding backend (hashed | model2vec), saved in .knobyte/config.json

Options:
  -h, --help  Print help
```

#### `knobyte cozo model pull`

Download a Model2Vec model from Hugging Face into ~/.knobyte/models (explicit only)

```text
Usage: knobyte cozo model pull [OPTIONS]

Options:
      --model <MODEL>  Hugging Face repo id [default: minishlab/potion-base-8M]
      --force          Re-download files that are already present
      --json           
  -h, --help           Print help
```

#### `knobyte cozo model status`

Show the active embedding backend, model path, dimension and index state

```text
Usage: knobyte cozo model status [OPTIONS]

Options:
      --json  
  -h, --help  Print help
```

#### `knobyte cozo model use`

Select the embedding backend (hashed | model2vec), saved in .knobyte/config.json

```text
Usage: knobyte cozo model use [OPTIONS] <BACKEND>

Arguments:
  <BACKEND>  

Options:
      --model <MODEL>  Hugging Face repo id of the Model2Vec model (model2vec only)
      --json           
  -h, --help           Print help
```

## `knobyte setup`

Set up Knobyte project memory: scaffold, AI tool anchors, skills, graph, population

```text
Usage: knobyte setup [OPTIONS]

Options:
      --dry-run            Show what would happen without making changes
      --mode <MODE>        code-repo, agent-memory, monorepo or docs-only (default: the saved mode, else code-repo)
      --cli                Run the interactive terminal flow (tool menu, confirmations)
      --tools <TOOLS>      AI tools to configure: claude, cursor, windsurf, copilot, opencode, codex (comma separated, or none)
      --launch-agent       Launch Claude Code / Codex to populate the scaffold without asking (explicit consent)
      --no-agent           Never launch an agent; print the population prompt instead
      --agent <AGENT>      Agent to launch: claude or codex (default: first selected tool that is installed)
      --skip-graph         Do not build the code graph
      --commit             Create the commit checkpoint without asking
      --backup-skills      Move conflicting skill directories aside instead of stopping
      --capture-baselines  On an already populated scaffold, also capture missing grounding baselines into the Markdown (a re-run otherwise leaves tracked files unchanged)
  -h, --help               Print help
```

## `knobyte init`

Print a pre-analysed brief of the repository (manifests, entry points, folders, tooling)

```text
Usage: knobyte init [OPTIONS]

Options:
      --json  
  -h, --help  Print help
```

## `knobyte update`

Refresh Knobyte-owned scaffold files and managed blocks without touching populated content

```text
Usage: knobyte update [OPTIONS]

Options:
      --dry-run  
      --json     
  -h, --help     Print help
```

## `knobyte check`

Check project memory drift against current codebase

```text
Usage: knobyte check [OPTIONS]

Options:
      --quiet                    Single-line summary only
      --json                     Output the full drift report as JSON
      --fix                      Rewrite moved grounding references (shows the plan and asks first), then hand remaining errors to the `sync` repair flow
  -y, --yes                      With --fix: apply the planned changes without asking (required when not interactive or with --json)
      --dry-run                  With --fix: print the planned changes only; write nothing
      --verbose                  Show detailed diagnostic output (files scanned, claims, issues per checker)
      --stale-warn-days <N>      Warn when a file hasn't changed in N days (default 30)
      --stale-error-days <N>     Error when a file hasn't changed in N days (default 90)
      --stale-warn-commits <N>   Warn when a file has N commits since its last change (default 50)
      --stale-error-commits <N>  Error when a file has N commits since its last change (default 200)
  -h, --help                     Print help
```

## `knobyte sync`

Repair drift: relocate moved groundings, then fix files with an agent or printed prompts

```text
Usage: knobyte sync [OPTIONS]

Options:
      --dry-run                  Print the repair prompt without launching anything
      --warnings                 Include warning-only files (by default only files with errors)
      --launch-agent             Launch Claude Code / Codex without the interactive menu (explicit consent)
      --agent <AGENT>            Agent to launch: claude or codex
      --print-prompt             Print the repair prompt to paste into your agent and exit
      --accept                   Capture grounding baselines for repaired files without asking
      --max-cycles <MAX_CYCLES>  Maximum repair cycles [default: 3]
  -h, --help                     Print help
```

## `knobyte graph`

Build or query the deterministic code graph (bare `knobyte graph` builds it)

```text
Usage: knobyte graph [OPTIONS] [COMMAND]

Commands:
  status   Read-only health: fresh, stale, degraded, corrupt, rebuild_required or missing
  refresh  Incremental refresh: re-extract only changed files, publish atomically
  rebuild  Full rebuild into an isolated candidate, published atomically
  query    Structural lookup: where-defined, who-calls, what-calls, who-imports
  scope    Task-scoped retrieval: ranked files, source, directed flows and facts in one response
  get      Nodes by id or grounding reference, optionally with their source
  ground   Grounding baselines and retro-grounding (default: re-baseline grounded references)
  repair   Repair the graph store in place (WAL recovery, index/FTS rebuild, schema upgrade, dangling rows)

Options:
      --root <ROOT>  Project root to build (bare `knobyte graph` only; defaults to the discovered project)
      --json         
      --ts-compiler  Resolve TypeScript/JavaScript with the TypeScript type checker (needs Node and a `typescript` package; falls back to source-only extraction when unavailable)
  -h, --help         Print help
```

### `knobyte graph status`

Read-only health: fresh, stale, degraded, corrupt, rebuild_required or missing

```text
Usage: knobyte graph status [OPTIONS]

Options:
      --root <ROOT>  
      --json         
  -h, --help         Print help
```

### `knobyte graph refresh`

Incremental refresh: re-extract only changed files, publish atomically

```text
Usage: knobyte graph refresh [OPTIONS]

Options:
      --root <ROOT>             
      --json                    
      --ts-compiler             Resolve TypeScript/JavaScript with the TypeScript type checker (needs Node and a `typescript` package; falls back to source-only extraction when unavailable)
      --lock-timeout <SECONDS>  Wait up to this many seconds for a concurrent rebuild / refresh / repair
  -h, --help                    Print help
```

### `knobyte graph rebuild`

Full rebuild into an isolated candidate, published atomically

```text
Usage: knobyte graph rebuild [OPTIONS]

Options:
      --root <ROOT>             
      --json                    
      --ts-compiler             Resolve TypeScript/JavaScript with the TypeScript type checker (needs Node and a `typescript` package; falls back to source-only extraction when unavailable)
      --lock-timeout <SECONDS>  Wait up to this many seconds for a concurrent rebuild / refresh / repair
  -h, --help                    Print help
```

### `knobyte graph query`

Structural lookup: where-defined, who-calls, what-calls, who-imports

```text
Usage: knobyte graph query [OPTIONS] <RELATION> <TARGET>

Arguments:
  <RELATION>  
  <TARGET>    

Options:
      --json
          
      --jsonl
          Agent protocol v3 JSONL (also selected by any budget/detail flag)
      --detail <DETAIL>
          Detail level: minimal, standard or source
      --max-nodes <MAX_NODES>
          Maximum nodes to return
      --max-files <MAX_FILES>
          Maximum source files to return (scope)
      --max-flow-steps <MAX_FLOW_STEPS>
          Maximum directed flow steps (scope)
      --max-output-tokens <MAX_OUTPUT_TOKENS>
          Hard output token ceiling (estimated, 4 characters per token)
      --max-source-lines <MAX_SOURCE_LINES>
          Per-node source line cap (with --detail source)
      --fingerprint
          Attach body hashes and serialized MinHash fingerprints to facts (grounding workflow)
  -h, --help
          Print help
```

### `knobyte graph scope`

Task-scoped retrieval: ranked files, source, directed flows and facts in one response

```text
Usage: knobyte graph scope [OPTIONS] [TASKS]...

Arguments:
  [TASKS]...  

Options:
      --json
          Protocol records as a JSON array
      --jsonl
          Agent protocol v3 JSONL
      --wiki
          Attach wiki entities grounded to the returned nodes
      --hybrid
          Re-rank with Cozo vector similarity (optional; falls back to lexical ranking)
      --detail <DETAIL>
          Detail level: minimal, standard or source
      --max-nodes <MAX_NODES>
          Maximum nodes to return
      --max-files <MAX_FILES>
          Maximum source files to return (scope)
      --max-flow-steps <MAX_FLOW_STEPS>
          Maximum directed flow steps (scope)
      --max-output-tokens <MAX_OUTPUT_TOKENS>
          Hard output token ceiling (estimated, 4 characters per token)
      --max-source-lines <MAX_SOURCE_LINES>
          Per-node source line cap (with --detail source)
      --fingerprint
          Attach body hashes and serialized MinHash fingerprints to facts (grounding workflow)
  -h, --help
          Print help
```

### `knobyte graph get`

Nodes by id or grounding reference, optionally with their source

```text
Usage: knobyte graph get [OPTIONS] [IDS]...

Arguments:
  [IDS]...  

Options:
      --source
          Include each node's source lines
      --max-lines <MAX_LINES>
          Maximum source lines per node (implies --source; capped at 400)
      --json
          
      --jsonl
          Agent protocol v3 JSONL (also selected by any budget/detail flag)
      --detail <DETAIL>
          Detail level: minimal, standard or source
      --max-nodes <MAX_NODES>
          Maximum nodes to return
      --max-files <MAX_FILES>
          Maximum source files to return (scope)
      --max-flow-steps <MAX_FLOW_STEPS>
          Maximum directed flow steps (scope)
      --max-output-tokens <MAX_OUTPUT_TOKENS>
          Hard output token ceiling (estimated, 4 characters per token)
      --max-source-lines <MAX_SOURCE_LINES>
          Per-node source line cap (with --detail source)
      --fingerprint
          Attach body hashes and serialized MinHash fingerprints to facts (grounding workflow)
  -h, --help
          Print help
```

### `knobyte graph ground`

Grounding baselines and retro-grounding (default: re-baseline grounded references)

```text
Usage: knobyte graph ground [OPTIONS]

Options:
      --rebaseline               Re-baseline grounded references to the current code (the default mode)
      --dry-run                  Propose groundings for wiki entities that have none, without writing
      --apply                    Write proposed groundings into the documents, then re-baseline
      --agent                    Agent-led retro-grounding (with --dry-run: print the agent prompt). Shows the command and asks before launching unless --launch-agent is given
      --launch-agent             With --agent: launch without asking (still disabled by KNOBYTE_NO_AGENT_LAUNCH)
      --per-entity <PER_ENTITY>  Maximum proposed references per entity [default: 3]
      --json                     
  -h, --help                     Print help
```

### `knobyte graph repair`

Repair the graph store in place (WAL recovery, index/FTS rebuild, schema upgrade, dangling rows)

```text
Usage: knobyte graph repair [OPTIONS]

Options:
      --root <ROOT>             
      --json                    
      --lock-timeout <SECONDS>  Wait up to this many seconds for a concurrent rebuild / refresh / repair
  -h, --help                    Print help
```

## `knobyte impact`

Find blast radius / impact of a symbol, file or grounding reference

```text
Usage: knobyte impact [OPTIONS] <TARGET>

Arguments:
  <TARGET>  

Options:
      --depth <DEPTH>
          Transitive depth to follow (1-8) [default: 3]
      --callers-only
          Follow only call/instantiation edges (transitive callers)
      --root <ROOT>
          
      --json
          
      --jsonl
          Agent protocol v3 JSONL (also selected by any budget/detail flag)
      --detail <DETAIL>
          Detail level: minimal, standard or source
      --max-nodes <MAX_NODES>
          Maximum nodes to return
      --max-files <MAX_FILES>
          Maximum source files to return (scope)
      --max-flow-steps <MAX_FLOW_STEPS>
          Maximum directed flow steps (scope)
      --max-output-tokens <MAX_OUTPUT_TOKENS>
          Hard output token ceiling (estimated, 4 characters per token)
      --max-source-lines <MAX_SOURCE_LINES>
          Per-node source line cap (with --detail source)
      --fingerprint
          Attach body hashes and serialized MinHash fingerprints to facts (grounding workflow)
  -h, --help
          Print help
```

## `knobyte wiki`

Build, query, validate, edit (apply) and synthesize the project Wiki

```text
Usage: knobyte wiki <COMMAND>

Commands:
  list              List entities (archived hidden unless --include-archived)
  show              Show one entity with relations, backlinks, groundings and sources
  query             Ranked search: id > title > summary > body
  related           Bounded neighbourhood of an entity
  backlinks         Entities that relate to this one
  for-code          Entities grounded in code symbols (graph ids or readable refs)
  graph             Bounded slice of the entity graph (around the given ids, or the first --limit entities)
  trace             Spec → requirement → decision → component → code → test traceability
  validate          Validate the wiki Markdown (no index needed)
  rebuild-index     Rebuild the wiki search index (and sync CozoDB)
  apply             Apply typed wiki operations from a JSON file (object, array, or JSONL)
  regenerate-views  Rewrite stale `<!-- kb:generated:begin -->` sections
  synthesis         Agent-driven synthesis of wiki knowledge from the code graph
  migrate           Rewrite older Knobyte wiki formats (legacy statuses, `document` types, derived ids, old grounding shapes, legacy `edges`) through audited operations. Plans only unless --apply
  index             Index maintenance: state, normalized dump, doctor

Options:
  -h, --help  Print help
```

### `knobyte wiki list`

List entities (archived hidden unless --include-archived)

```text
Usage: knobyte wiki list [OPTIONS]

Options:
      --type <TYPES>      Only these entity types (repeatable or comma-separated)
      --topic <TOPIC>     Only members of this topic (id, title or alias)
      --status <STATUS>   Only these lifecycle states: in_flight, promoted, deprecated, archived
      --health <HEALTH>   Only these grounding health values: fresh, unverified, ambiguous, changed, missing, none
      --limit <LIMIT>     Maximum results (default 50, max 500)
      --include-archived  Include archived entities (hidden by default)
      --offset <OFFSET>   Skip this many results (paging; continue while `truncated`) [default: 0]
      --json              
      --jsonl             One JSON object per line
  -h, --help              Print help
```

### `knobyte wiki show`

Show one entity with relations, backlinks, groundings and sources

```text
Usage: knobyte wiki show [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --no-body          Omit the body
      --limit <LIMIT>    Page size for relations and backlinks (default 25, max 200)
      --offset <OFFSET>  Skip this many relations and backlinks (use the reported nextOffset) [default: 0]
      --json             
  -h, --help             Print help
```

### `knobyte wiki query`

Ranked search: id > title > summary > body

```text
Usage: knobyte wiki query [OPTIONS] [TEXT]...

Arguments:
  [TEXT]...  

Options:
      --type <TYPES>      Only these entity types (repeatable or comma-separated)
      --topic <TOPIC>     Only members of this topic (id, title or alias)
      --status <STATUS>   Only these lifecycle states: in_flight, promoted, deprecated, archived
      --health <HEALTH>   Only these grounding health values: fresh, unverified, ambiguous, changed, missing, none
      --limit <LIMIT>     Maximum results (default 50, max 500)
      --include-archived  Include archived entities (hidden by default)
      --offset <OFFSET>   Skip this many results (paging; continue while `truncated`) [default: 0]
      --json              
      --jsonl             
  -h, --help              Print help
```

### `knobyte wiki related`

Bounded neighbourhood of an entity

```text
Usage: knobyte wiki related [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --depth <DEPTH>            Traversal depth (default 2, max 5)
      --max-tokens <MAX_TOKENS>  Token budget for the reached entities (default 4000)
      --limit <LIMIT>            
      --include-archived         
      --json                     
  -h, --help                     Print help
```

### `knobyte wiki backlinks`

Entities that relate to this one

```text
Usage: knobyte wiki backlinks [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --limit <LIMIT>    Page size (default 25, max 200)
      --offset <OFFSET>  Skip this many backlinks (use the reported nextOffset) [default: 0]
      --json             
  -h, --help             Print help
```

### `knobyte wiki for-code`

Entities grounded in code symbols (graph ids or readable refs)

```text
Usage: knobyte wiki for-code [OPTIONS] <NODE_IDS>...

Arguments:
  <NODE_IDS>...  

Options:
      --limit <LIMIT>     
      --include-archived  Include archived entities (hidden by default)
      --json              
      --jsonl             
  -h, --help              Print help
```

### `knobyte wiki graph`

Bounded slice of the entity graph (around the given ids, or the first --limit entities)

```text
Usage: knobyte wiki graph [OPTIONS] [IDS]...

Arguments:
  [IDS]...  

Options:
      --depth <DEPTH>     
      --limit <LIMIT>     
      --include-archived  
      --json              
  -h, --help              Print help
```

### `knobyte wiki trace`

Spec → requirement → decision → component → code → test traceability

```text
Usage: knobyte wiki trace [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  
  -h, --help  Print help
```

### `knobyte wiki validate`

Validate the wiki Markdown (no index needed)

```text
Usage: knobyte wiki validate [OPTIONS]

Options:
      --json           
      --limit <LIMIT>  Maximum diagnostics shown
      --strict         Accepted for compatibility: validate always exits non-zero on error-severity findings
  -h, --help           Print help
```

### `knobyte wiki rebuild-index`

Rebuild the wiki search index (and sync CozoDB)

```text
Usage: knobyte wiki rebuild-index [OPTIONS]

Options:
      --incremental  Only re-read files whose content hash changed
      --json         
  -h, --help         Print help
```

### `knobyte wiki apply`

Apply typed wiki operations from a JSON file (object, array, or JSONL)

```text
Usage: knobyte wiki apply [OPTIONS] <FILE>

Arguments:
  <FILE>  

Options:
      --dry-run  Plan and print the changes without writing anything
      --json     
  -h, --help     Print help
```

### `knobyte wiki regenerate-views`

Rewrite stale `<!-- kb:generated:begin -->` sections

```text
Usage: knobyte wiki regenerate-views [OPTIONS]

Options:
      --dry-run  
      --json     
  -h, --help     Print help
```

### `knobyte wiki synthesis`

Agent-driven synthesis of wiki knowledge from the code graph

```text
Usage: knobyte wiki synthesis <COMMAND>

Commands:
  build    Discover clusters and produce the agent playbook
  prepare  Deterministic context and prompts for one stage
  propose  Validate an agent response into operation plans (writes only with --apply)

Options:
  -h, --help  Print help
```

#### `knobyte wiki synthesis build`

Discover clusters and produce the agent playbook

```text
Usage: knobyte wiki synthesis build [OPTIONS]

Options:
      --cluster <CLUSTER>  
      --print              Print the playbook instead of saving it under .knobyte/local/synthesis/
      --json               
  -h, --help               Print help
```

#### `knobyte wiki synthesis prepare`

Deterministic context and prompts for one stage

```text
Usage: knobyte wiki synthesis prepare [OPTIONS] --stage <STAGE>

Options:
      --stage <STAGE>      architecture_component | pattern | convention | global | relationships
      --cluster <CLUSTER>  
      --json               
  -h, --help               Print help
```

#### `knobyte wiki synthesis propose`

Validate an agent response into operation plans (writes only with --apply)

```text
Usage: knobyte wiki synthesis propose [OPTIONS] <FILE>

Arguments:
  <FILE>  

Options:
      --apply          
      --stage <STAGE>  
      --json           
  -h, --help           Print help
```

### `knobyte wiki migrate`

Rewrite older Knobyte wiki formats (legacy statuses, `document` types, derived ids, old grounding shapes, legacy `edges`) through audited operations. Plans only unless --apply

```text
Usage: knobyte wiki migrate [OPTIONS]

Options:
      --dry-run  Plan and report without writing (the default)
      --apply    Apply the planned migration
      --json     
  -h, --help     Print help
```

### `knobyte wiki index`

Index maintenance: state, normalized dump, doctor

```text
Usage: knobyte wiki index <COMMAND>

Commands:
  status  The index state (missing, fresh, stale, degraded, rebuild_required, corrupt, migration_required) with its revision
  dump    Deterministic normalized dump of the index (rows ordered by key, wall-clock excluded)
  doctor  Integrity check plus a diff of the index against a clean rebuild of the Markdown

Options:
  -h, --help  Print help
```

#### `knobyte wiki index status`

The index state (missing, fresh, stale, degraded, rebuild_required, corrupt, migration_required) with its revision

```text
Usage: knobyte wiki index status [OPTIONS]

Options:
      --json  
  -h, --help  Print help
```

#### `knobyte wiki index dump`

Deterministic normalized dump of the index (rows ordered by key, wall-clock excluded)

```text
Usage: knobyte wiki index dump [OPTIONS]

Options:
      --out <OUT>  Write the dump to a file instead of stdout
      --json       
  -h, --help       Print help
```

#### `knobyte wiki index doctor`

Integrity check plus a diff of the index against a clean rebuild of the Markdown

```text
Usage: knobyte wiki index doctor [OPTIONS]

Options:
      --json  
  -h, --help  Print help
```

## `knobyte export`

Export the whole scaffold as one Markdown bundle (stdout, or --out PATH)

```text
Usage: knobyte export [OPTIONS]

Options:
      --out <OUT>  
  -h, --help       Print help
```

## `knobyte member`

Manage team member attribution and identity

```text
Usage: knobyte member <COMMAND>

Commands:
  contract    Versioned JSON Schema catalog of member request files
  list        List canonical team members
  show        Show one member
  current     Show the effective actor, how it was resolved, and the local selection
  add         Register a new team member (name/email default to git config)
  update      Update a member's display name, email, role or Git aliases
  deactivate  Mark a member inactive (history is kept)
  reactivate  Restore an inactive member
  select      Select this checkout's current member
  clear       Clear this checkout's member selection

Options:
  -h, --help  Print help
```

### `knobyte member contract`

Versioned JSON Schema catalog of member request files

```text
Usage: knobyte member contract [OPTIONS]

Options:
      --action <ACTION>  Only this action (e.g. member.add)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

### `knobyte member list`

List canonical team members

```text
Usage: knobyte member list [OPTIONS]

Options:
      --active           Only active members
      --inactive         Only inactive members
      --cursor <CURSOR>  Continue a bounded result page
      --limit <LIMIT>    Maximum results (1-100, default 50)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

### `knobyte member show`

Show one member

```text
Usage: knobyte member show [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

### `knobyte member current`

Show the effective actor, how it was resolved, and the local selection

```text
Usage: knobyte member current [OPTIONS]

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

### `knobyte member add`

Register a new team member (name/email default to git config)

```text
Usage: knobyte member add [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --name <NAME>        
      --email <EMAIL>      
      --role <ROLE>        
      --alias <ALIASES>    Git alias "Name <email>" (repeatable; defaults to name+email)
      --select             Also select the new member as the current member of this checkout
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte member update`

Update a member's display name, email, role or Git aliases

```text
Usage: knobyte member update [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --name <NAME>        
      --email <EMAIL>      New email ("" clears it)
      --role <ROLE>        New role ("" clears it)
      --alias <ALIASES>    Replace Git aliases with these ("Name <email>", repeatable)
      --clear-aliases      Remove all Git aliases
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte member deactivate`

Mark a member inactive (history is kept)

```text
Usage: knobyte member deactivate [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte member reactivate`

Restore an inactive member

```text
Usage: knobyte member reactivate [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte member select`

Select this checkout's current member

```text
Usage: knobyte member select [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte member clear`

Clear this checkout's member selection

```text
Usage: knobyte member clear [OPTIONS]

Options:
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

## `knobyte activity`

View canonical Activity history

```text
Usage: knobyte activity <COMMAND>

Commands:
  contract  Versioned JSON Schema catalog of activity request files
  list      List canonical activity, newest first
  show      Show one activity record
  record    Record a custom activity event
  timeline  Merged timeline of canonical activity and the decision/event log

Options:
  -h, --help  Print help
```

### `knobyte activity contract`

Versioned JSON Schema catalog of activity request files

```text
Usage: knobyte activity contract [OPTIONS]

Options:
      --action <ACTION>  Only this action (e.g. activity.record)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

### `knobyte activity list`

List canonical activity, newest first

```text
Usage: knobyte activity list [OPTIONS]

Options:
      --since <SINCE>    Only records at or after: RFC 3339, YYYY-MM-DD, or relative Nd/Nh
      --cursor <CURSOR>  Continue a bounded result page
      --limit <LIMIT>    Maximum results (1-100, default 50)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

### `knobyte activity show`

Show one activity record

```text
Usage: knobyte activity show [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

### `knobyte activity record`

Record a custom activity event

```text
Usage: knobyte activity record [OPTIONS] [ACTION] [SUMMARY]

Arguments:
  [ACTION]   
  [SUMMARY]  

Options:
      --kind <KIND>              Entity kind of the subject (default: general)
      --target <TARGET>          Entity id of the subject
      --subject <SUBJECTS>       Typed subject: entity:<kind>:<id>, code:<symbol>, file:<path>, commit:<hash> (repeatable)
      --workstream <WORKSTREAM>  Related workstream id
      --preview                  Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>         Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>           Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>        Stable operation id (exact replay is idempotent)
      --json                     Emit the schema v1 team envelope
  -h, --help                     Print help
```

### `knobyte activity timeline`

Merged timeline of canonical activity and the decision/event log

```text
Usage: knobyte activity timeline [OPTIONS]

Options:
      --source <SOURCE>  Only one source: activity or log
      --since <SINCE>    
      --cursor <CURSOR>  Continue a bounded result page
      --limit <LIMIT>    Maximum results (1-100, default 50)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

## `knobyte workstream`

Manage team workstreams

```text
Usage: knobyte workstream <COMMAND>

Commands:
  contract  Versioned JSON Schema catalog of workstream request files
  list      List workstreams (archived hidden unless requested)
  show      Show one workstream
  create    Create a workstream
  update    Update a workstream
  archive   Archive a workstream

Options:
  -h, --help  Print help
```

### `knobyte workstream contract`

Versioned JSON Schema catalog of workstream request files

```text
Usage: knobyte workstream contract [OPTIONS]

Options:
      --action <ACTION>  Only this action (e.g. workstream.create)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

### `knobyte workstream list`

List workstreams (archived hidden unless requested)

```text
Usage: knobyte workstream list [OPTIONS]

Options:
      --state <STATES>    Filter by lifecycle state (repeatable)
      --include-archived  
      --cursor <CURSOR>   Continue a bounded result page
      --limit <LIMIT>     Maximum results (1-100, default 50)
      --json              Emit the schema v1 team envelope
  -h, --help              Print help
```

### `knobyte workstream show`

Show one workstream

```text
Usage: knobyte workstream show [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

### `knobyte workstream create`

Create a workstream

```text
Usage: knobyte workstream create [OPTIONS] [ID] [TITLE]

Arguments:
  [ID]     
  [TITLE]  

Options:
      --description <DESCRIPTION>
          
      --goal <GOAL>
          
      --summary <SUMMARY>
          
      --state <STATE>
          planned, active, blocked or done
      --owner <OWNERS>
          Owner member id (repeatable)
      --contributor <CONTRIBUTORS>
          
      --path <PATHS>
          Repository path in scope (repeatable)
      --code <CODE>
          Code symbol in scope (repeatable)
      --topic <TOPICS>
          
      --component <COMPONENTS>
          
      --related <RELATED>
          
      --next-milestone <NEXT_MILESTONE>
          
      --preview
          Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>
          Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>
          Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>
          Stable operation id (exact replay is idempotent)
      --json
          Emit the schema v1 team envelope
  -h, --help
          Print help
```

### `knobyte workstream update`

Update a workstream

```text
Usage: knobyte workstream update [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --title <TITLE>
          
      --description <DESCRIPTION>
          
      --goal <GOAL>
          
      --summary <SUMMARY>
          
      --state <STATE>
          planned, active, blocked or done
      --owner <OWNERS>
          Owner member id (repeatable)
      --contributor <CONTRIBUTORS>
          
      --path <PATHS>
          Repository path in scope (repeatable)
      --code <CODE>
          Code symbol in scope (repeatable)
      --topic <TOPICS>
          
      --component <COMPONENTS>
          
      --related <RELATED>
          
      --next-milestone <NEXT_MILESTONE>
          
      --blocker <BLOCKERS>
          Replace blockers (repeatable)
      --clear-blockers
          
      --current-state <CURRENT_STATE>
          
      --preview
          Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>
          Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>
          Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>
          Stable operation id (exact replay is idempotent)
      --json
          Emit the schema v1 team envelope
  -h, --help
          Print help
```

### `knobyte workstream archive`

Archive a workstream

```text
Usage: knobyte workstream archive [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

## `knobyte spec`

List or view requirements specs

```text
Usage: knobyte spec <COMMAND>

Commands:
  list  List specs with lifecycle and grounding health
  show  Show a spec (id or path such as specs/auth.md) with its requirements, acceptance criteria, constraints and grounding rollup

Options:
  -h, --help  Print help
```

### `knobyte spec list`

List specs with lifecycle and grounding health

```text
Usage: knobyte spec list [OPTIONS]

Options:
      --lifecycle <LIFECYCLE>  in_flight, promoted, deprecated or archived
      --grounding <GROUNDING>  fresh, changed, missing, ambiguous or unverified
      --topic <TOPIC>          
      --include-archived       
      --cursor <CURSOR>        Continue a bounded result page
      --limit <LIMIT>          Maximum results (1-100, default 50)
      --json                   Emit the schema v1 team envelope
  -h, --help                   Print help
```

### `knobyte spec show`

Show a spec (id or path such as specs/auth.md) with its requirements, acceptance criteria, constraints and grounding rollup

```text
Usage: knobyte spec show [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

## `knobyte inbox`

Propose additions or corrections to project memory

```text
Usage: knobyte inbox <COMMAND>

Commands:
  target    Resolve an existing knowledge record and its exact revision for a correction
  contract  Versioned JSON Schema catalog of Inbox request files
  draft     Local inbox drafts (list, show, save, delete)
  publish   Publish a local draft as a pending proposal
  proposal  Published proposals (list, show, approve, reject, withdraw, mark-stale, repair)
  approve   Approve a pending proposal (shorthand for `inbox proposal approve`)
  reject    Reject a pending proposal (shorthand for `inbox proposal reject`)
  withdraw  Withdraw your own pending proposal (shorthand)

Options:
  -h, --help  Print help
```

### `knobyte inbox target`

Resolve an existing knowledge record and its exact revision for a correction

```text
Usage: knobyte inbox target [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

### `knobyte inbox contract`

Versioned JSON Schema catalog of Inbox request files

```text
Usage: knobyte inbox contract [OPTIONS]

Options:
      --action <ACTION>  Only this action (e.g. inbox.draft.save)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

### `knobyte inbox draft`

Local inbox drafts (list, show, save, delete)

```text
Usage: knobyte inbox draft <COMMAND>

Commands:
  list    List local drafts
  show    Show one complete local draft
  save    Save (create or, with --draft-id, replace) a local draft
  delete  Delete a local draft

Options:
  -h, --help  Print help
```

#### `knobyte inbox draft list`

List local drafts

```text
Usage: knobyte inbox draft list [OPTIONS]

Options:
      --cursor <CURSOR>  Continue a bounded result page
      --limit <LIMIT>    Maximum results (1-100, default 50)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

#### `knobyte inbox draft show`

Show one complete local draft

```text
Usage: knobyte inbox draft show [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

#### `knobyte inbox draft save`

Save (create or, with --draft-id, replace) a local draft

```text
Usage: knobyte inbox draft save [OPTIONS]

Options:
      --draft-id <DRAFT_ID>
          Replace this existing draft
      --title <TITLE>
          
      --target <TARGET>
          Legacy Markdown edit: target document under .knobyte/ (e.g. context/rate-limit.md)
      --content <CONTENT>
          Legacy Markdown edit: content to append or replace
      --mode <MODE>
          Legacy Markdown edit: append (default) or replace
      --change <CHANGE>
          Typed change: knowledge.create, knowledge.update, spec.create or spec.update
      --kind <ENTITY_KIND>
          Entity kind for *.create (architecture, component, convention, decision, pattern, guide; spec, requirement, constraint, acceptance_criterion)
      --entity <ENTITY>
          Target entity id for *.update (see `knobyte inbox target <id>`)
      --body <BODY>
          Markdown body for a create, or the replacement body for an update
      --summary <SUMMARY>
          
      --status <STATUS>
          in_flight (default) or promoted, for *.create
      --topic <TOPICS>
          
      --relation <RELATION>
          spec.create relation <type>:<target-id> (derived_from, refines, constrained_by, verified_by)
      --reason <REASON>
          Why this change matters
      --evidence <EVIDENCE>
          Evidence: entity:<id>, code:<symbol>, commit:<hash>, file:<path>, URL, or a note (repeatable)
      --target-revision <TARGET_REVISION>
          Pin the target revision read with `inbox target` (sha256:...)
      --preview
          Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>
          Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>
          Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>
          Stable operation id (exact replay is idempotent)
      --json
          Emit the schema v1 team envelope
  -h, --help
          Print help
```

#### `knobyte inbox draft delete`

Delete a local draft

```text
Usage: knobyte inbox draft delete [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte inbox publish`

Publish a local draft as a pending proposal

```text
Usage: knobyte inbox publish [OPTIONS] [DRAFT_ID]

Arguments:
  [DRAFT_ID]  

Options:
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte inbox proposal`

Published proposals (list, show, approve, reject, withdraw, mark-stale, repair)

```text
Usage: knobyte inbox proposal <COMMAND>

Commands:
  list        List proposals
  show        Show one proposal
  approve     Approve a pending proposal and apply its knowledge change
  reject      Reject a pending proposal
  withdraw    Withdraw your own pending proposal
  mark-stale  Mark a pending proposal stale after its target changed
  repair      Repair a stale proposal back to pending with new content

Options:
  -h, --help  Print help
```

#### `knobyte inbox proposal list`

List proposals

```text
Usage: knobyte inbox proposal list [OPTIONS]

Options:
      --state <STATES>   Filter by state: pending, approved, rejected, withdrawn, stale (repeatable)
      --cursor <CURSOR>  Continue a bounded result page
      --limit <LIMIT>    Maximum results (1-100, default 50)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

#### `knobyte inbox proposal show`

Show one proposal

```text
Usage: knobyte inbox proposal show [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

#### `knobyte inbox proposal approve`

Approve a pending proposal and apply its knowledge change

```text
Usage: knobyte inbox proposal approve [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --note <NOTE>        Review rationale
      --member <MEMBER>    Expected reviewer: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
      --self-approve       Approve your own proposal without teammate review
  -h, --help               Print help
```

#### `knobyte inbox proposal reject`

Reject a pending proposal

```text
Usage: knobyte inbox proposal reject [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --note <NOTE>        Review rationale
      --member <MEMBER>    Expected reviewer: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

#### `knobyte inbox proposal withdraw`

Withdraw your own pending proposal

```text
Usage: knobyte inbox proposal withdraw [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --note <NOTE>        Review rationale
      --member <MEMBER>    Expected reviewer: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

#### `knobyte inbox proposal mark-stale`

Mark a pending proposal stale after its target changed

```text
Usage: knobyte inbox proposal mark-stale [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --note <NOTE>        Review rationale
      --member <MEMBER>    Expected reviewer: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

#### `knobyte inbox proposal repair`

Repair a stale proposal back to pending with new content

```text
Usage: knobyte inbox proposal repair [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --from-draft <DRAFT_ID>
          Use this local draft's content as the replacement
      --title <TITLE>
          
      --target <TARGET>
          Legacy Markdown edit: target document under .knobyte/ (e.g. context/rate-limit.md)
      --content <CONTENT>
          Legacy Markdown edit: content to append or replace
      --mode <MODE>
          Legacy Markdown edit: append (default) or replace
      --change <CHANGE>
          Typed change: knowledge.create, knowledge.update, spec.create or spec.update
      --kind <ENTITY_KIND>
          Entity kind for *.create (architecture, component, convention, decision, pattern, guide; spec, requirement, constraint, acceptance_criterion)
      --entity <ENTITY>
          Target entity id for *.update (see `knobyte inbox target <id>`)
      --body <BODY>
          Markdown body for a create, or the replacement body for an update
      --summary <SUMMARY>
          
      --status <STATUS>
          in_flight (default) or promoted, for *.create
      --topic <TOPICS>
          
      --relation <RELATION>
          spec.create relation <type>:<target-id> (derived_from, refines, constrained_by, verified_by)
      --reason <REASON>
          Why this change matters
      --evidence <EVIDENCE>
          Evidence: entity:<id>, code:<symbol>, commit:<hash>, file:<path>, URL, or a note (repeatable)
      --target-revision <TARGET_REVISION>
          Pin the target revision read with `inbox target` (sha256:...)
      --preview
          Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>
          Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>
          Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>
          Stable operation id (exact replay is idempotent)
      --json
          Emit the schema v1 team envelope
  -h, --help
          Print help
```

### `knobyte inbox approve`

Approve a pending proposal (shorthand for `inbox proposal approve`)

```text
Usage: knobyte inbox approve [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --note <NOTE>        Review rationale
      --member <MEMBER>    Expected reviewer: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
      --self-approve       Approve your own proposal without teammate review
  -h, --help               Print help
```

### `knobyte inbox reject`

Reject a pending proposal (shorthand for `inbox proposal reject`)

```text
Usage: knobyte inbox reject [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --note <NOTE>        Review rationale
      --member <MEMBER>    Expected reviewer: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte inbox withdraw`

Withdraw your own pending proposal (shorthand)

```text
Usage: knobyte inbox withdraw [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --note <NOTE>        Review rationale
      --member <MEMBER>    Expected reviewer: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

## `knobyte relay`

Prepare and exchange context handoffs (relays)

```text
Usage: knobyte relay <COMMAND>

Commands:
  contract     Versioned JSON Schema catalog of Relay request files
  draft        Local relay drafts (list, show, save, delete)
  list         List relays
  publish      Publish a local draft (captures branch/HEAD)
  show         Show one relay
  acknowledge  Claim a published relay
  close        Close an acknowledged relay (sender or claimant)

Options:
  -h, --help  Print help
```

### `knobyte relay contract`

Versioned JSON Schema catalog of Relay request files

```text
Usage: knobyte relay contract [OPTIONS]

Options:
      --action <ACTION>  
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

### `knobyte relay draft`

Local relay drafts (list, show, save, delete)

```text
Usage: knobyte relay draft <COMMAND>

Commands:
  list    List local relay drafts
  show    Show one complete local relay draft
  save    Save (create or, with --draft-id, replace) a local relay draft
  delete  Delete a local relay draft

Options:
  -h, --help  Print help
```

#### `knobyte relay draft list`

List local relay drafts

```text
Usage: knobyte relay draft list [OPTIONS]

Options:
      --cursor <CURSOR>  Continue a bounded result page
      --limit <LIMIT>    Maximum results (1-100, default 50)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

#### `knobyte relay draft show`

Show one complete local relay draft

```text
Usage: knobyte relay draft show [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

#### `knobyte relay draft save`

Save (create or, with --draft-id, replace) a local relay draft

```text
Usage: knobyte relay draft save [OPTIONS]

Options:
      --draft-id <DRAFT_ID>           Replace this existing draft
      --from <DRAFT_FILE>             Create the draft from sparse JSON content (fields of the relay draft contract)
      --sender <SENDER>               Expected actor: must be the current member (refused otherwise)
      --title <TITLE>                 
      --summary <SUMMARY>             
      --audience <AUDIENCE>           team (any active member may claim) or members (named recipients only)
      --to <TO>                       Named recipient member id (repeatable, at most 32)
      --completed <COMPLETED>         Completed work (repeatable)
      --in-progress <IN_PROGRESS>     Work in progress (repeatable)
      --progress <PROGRESS>           Legacy progress note (repeatable)
      --decision <DECISIONS>          Decision made (repeatable)
      --blocker <BLOCKERS>            
      --question <QUESTIONS>          Unresolved question (repeatable)
      --next <NEXT>                   Next action (repeatable)
      --changed-file <CHANGED_FILES>  Changed file (repeatable; default: detected from git)
      --no-auto-files                 Do not detect changed files from git
      --code <CODE>                   Code reference: symbol id or file:<path> (repeatable)
      --evidence <EVIDENCE>           Evidence: entity:<id>, code:<symbol>, commit:<hash>, file:<path>, URL, or a note (repeatable)
      --workstream <WORKSTREAM>       
      --preview                       Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>              Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>                Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>             Stable operation id (exact replay is idempotent)
      --json                          Emit the schema v1 team envelope
  -h, --help                          Print help
```

#### `knobyte relay draft delete`

Delete a local relay draft

```text
Usage: knobyte relay draft delete [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte relay list`

List relays

```text
Usage: knobyte relay list [OPTIONS]

Options:
      --perspective <PERSPECTIVE>  all (default), mine or sent
      --state <STATES>             published, acknowledged or closed (repeatable)
      --workstream <WORKSTREAM>    
      --cursor <CURSOR>            Continue a bounded result page
      --limit <LIMIT>              Maximum results (1-100, default 50)
      --json                       Emit the schema v1 team envelope
  -h, --help                       Print help
```

### `knobyte relay publish`

Publish a local draft (captures branch/HEAD)

```text
Usage: knobyte relay publish [OPTIONS] [DRAFT_ID]

Arguments:
  [DRAFT_ID]  

Options:
      --member <MEMBER>    Expected actor: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte relay show`

Show one relay

```text
Usage: knobyte relay show [OPTIONS] <RELAY_ID>

Arguments:
  <RELAY_ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

### `knobyte relay acknowledge`

Claim a published relay

```text
Usage: knobyte relay acknowledge [OPTIONS] [RELAY_ID]

Arguments:
  [RELAY_ID]  

Options:
      --member <MEMBER>    Expected actor: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte relay close`

Close an acknowledged relay (sender or claimant)

```text
Usage: knobyte relay close [OPTIONS] [RELAY_ID]

Arguments:
  [RELAY_ID]  

Options:
      --member <MEMBER>    Expected actor: must be the current member (refused otherwise)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

## `knobyte playbook`

Reusable team playbooks and their runs

```text
Usage: knobyte playbook <COMMAND>

Commands:
  contract  Versioned JSON Schema catalog of playbook request files
  list      List playbooks (archived hidden unless requested)
  show      Show a playbook with its steps and recent runs
  create    Create a playbook (draft unless --state active)
  update    Update a playbook; --step replaces the whole step list, --state active publishes a draft
  archive   Archive a playbook (archived playbooks are immutable and cannot be run)
  run       Start, inspect and advance playbook runs

Options:
  -h, --help  Print help
```

### `knobyte playbook contract`

Versioned JSON Schema catalog of playbook request files

```text
Usage: knobyte playbook contract [OPTIONS]

Options:
      --action <ACTION>  Only this action (e.g. playbook.run.complete-step)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

### `knobyte playbook list`

List playbooks (archived hidden unless requested)

```text
Usage: knobyte playbook list [OPTIONS]

Options:
      --state <STATES>    Filter by state: draft, active, archived (repeatable)
      --topic <TOPIC>     
      --include-archived  
      --cursor <CURSOR>   Continue a bounded result page
      --limit <LIMIT>     Maximum results (1-100, default 50)
      --json              Emit the schema v1 team envelope
  -h, --help              Print help
```

### `knobyte playbook show`

Show a playbook with its steps and recent runs

```text
Usage: knobyte playbook show [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

### `knobyte playbook create`

Create a playbook (draft unless --state active)

```text
Usage: knobyte playbook create [OPTIONS] [TITLE]

Arguments:
  [TITLE]  

Options:
      --id <ID>                       Explicit id (default: derived from the title)
      --summary <SUMMARY>             
      --trigger <TRIGGER>             When to use this playbook
      --state <STATE>                 draft or active (publishing = setting active)
      --owner <OWNERS>                Owner member id (repeatable)
      --topic <TOPICS>                
      --prerequisite <PREREQUISITES>  
      --related <RELATED>             
      --step <STEP>                   Step (repeatable, in order): "<title>[::<description>[::<evidence>;<evidence>...]]"
      --preview                       Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>              Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>                Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>             Stable operation id (exact replay is idempotent)
      --json                          Emit the schema v1 team envelope
  -h, --help                          Print help
```

### `knobyte playbook update`

Update a playbook; --step replaces the whole step list, --state active publishes a draft

```text
Usage: knobyte playbook update [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --title <TITLE>                 
      --summary <SUMMARY>             
      --trigger <TRIGGER>             When to use this playbook
      --state <STATE>                 draft or active (publishing = setting active)
      --owner <OWNERS>                Owner member id (repeatable)
      --topic <TOPICS>                
      --prerequisite <PREREQUISITES>  
      --related <RELATED>             
      --step <STEP>                   Step (repeatable, in order): "<title>[::<description>[::<evidence>;<evidence>...]]"
      --preview                       Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>              Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>                Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>             Stable operation id (exact replay is idempotent)
      --json                          Emit the schema v1 team envelope
  -h, --help                          Print help
```

### `knobyte playbook archive`

Archive a playbook (archived playbooks are immutable and cannot be run)

```text
Usage: knobyte playbook archive [OPTIONS] [ID]

Arguments:
  [ID]  

Options:
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte playbook run`

Start, inspect and advance playbook runs

```text
Usage: knobyte playbook run <COMMAND>

Commands:
  start          Start a run of an active playbook (its steps are snapshotted)
  list           List runs, newest first
  show           Show one run with its step states and evidence
  complete-step  Complete one pending step, recording evidence (the run completes with its last step)
  abandon        Abandon an active run

Options:
  -h, --help  Print help
```

#### `knobyte playbook run start`

Start a run of an active playbook (its steps are snapshotted)

```text
Usage: knobyte playbook run start [OPTIONS] [PLAYBOOK_ID]

Arguments:
  [PLAYBOOK_ID]  

Options:
      --workstream <WORKSTREAM>  Link the run to a workstream
      --title <TITLE>            Optional label for this run
      --preview                  Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>         Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>           Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>        Stable operation id (exact replay is idempotent)
      --json                     Emit the schema v1 team envelope
  -h, --help                     Print help
```

#### `knobyte playbook run list`

List runs, newest first

```text
Usage: knobyte playbook run list [OPTIONS]

Options:
      --playbook <PLAYBOOK>      
      --workstream <WORKSTREAM>  
      --state <STATES>           active, completed or abandoned (repeatable)
      --cursor <CURSOR>          Continue a bounded result page
      --limit <LIMIT>            Maximum results (1-100, default 50)
      --json                     Emit the schema v1 team envelope
  -h, --help                     Print help
```

#### `knobyte playbook run show`

Show one run with its step states and evidence

```text
Usage: knobyte playbook run show [OPTIONS] <RUN_ID>

Arguments:
  <RUN_ID>  

Options:
      --json  Emit the schema v1 team envelope
  -h, --help  Print help
```

#### `knobyte playbook run complete-step`

Complete one pending step, recording evidence (the run completes with its last step)

```text
Usage: knobyte playbook run complete-step [OPTIONS] [RUN_ID] [STEP_ID]

Arguments:
  [RUN_ID]   
  [STEP_ID]  Step id, or the step's 1-based number in the run

Options:
      --evidence <EVIDENCE>  Evidence: file:<path>, commit:<sha>, entity:<id>, code:<symbol>, a URL, or free text (repeatable)
      --note <NOTE>          
      --preview              Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>     Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>       Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>    Stable operation id (exact replay is idempotent)
      --json                 Emit the schema v1 team envelope
  -h, --help                 Print help
```

#### `knobyte playbook run abandon`

Abandon an active run

```text
Usage: knobyte playbook run abandon [OPTIONS] [RUN_ID]

Arguments:
  [RUN_ID]  

Options:
      --reason <REASON>    
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

## `knobyte catch-up`

What changed in shared memory since you last caught up (mark, reset)

```text
Usage: knobyte catch-up [OPTIONS]
       knobyte catch-up <COMMAND>

Commands:
  mark      Mark everything up to now (or --at) as seen: advances your checkout-local cursor
  reset     Reset your cursor (also adopts the current branch); --clear removes it
  contract  Versioned JSON Schema catalog of catch-up request files

Options:
      --since <SINCE>            Override the baseline: RFC 3339, YYYY-MM-DD, or relative Nd/Nh (default: your cursor, else 7d)
      --workstream <WORKSTREAM>  Only items related to this workstream
      --group <GROUPS>           Only these groups: handoffs, reviews, decisions, knowledge, workstreams, playbooks, activity (repeatable)
      --include-mine             Also list your own changes
      --cursor <CURSOR>          Continue a bounded result page
      --limit <LIMIT>            Maximum results (1-100, default 50)
      --json                     Emit the schema v1 team envelope
  -h, --help                     Print help
```

### `knobyte catch-up mark`

Mark everything up to now (or --at) as seen: advances your checkout-local cursor

```text
Usage: knobyte catch-up mark [OPTIONS]

Options:
      --at <AT>            RFC 3339 instant to mark up to (e.g. the digest's observedAt)
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte catch-up reset`

Reset your cursor (also adopts the current branch); --clear removes it

```text
Usage: knobyte catch-up reset [OPTIONS]

Options:
      --to <TO>            New baseline: RFC 3339, YYYY-MM-DD, or relative Nd/Nh (default: now)
      --clear              
      --preview            Print the signed preview envelope (exact changes) without writing anything
      --apply <ENVELOPE>   Apply the complete envelope printed by `--preview --json` (refused if anything changed)
      --request <FILE>     Take the action from a caller-authored request JSON file (see the `contract` subcommand)
      --operation-id <ID>  Stable operation id (exact replay is idempotent)
      --json               Emit the schema v1 team envelope
  -h, --help               Print help
```

### `knobyte catch-up contract`

Versioned JSON Schema catalog of catch-up request files

```text
Usage: knobyte catch-up contract [OPTIONS]

Options:
      --action <ACTION>  Only this action (catchup.mark or catchup.reset)
      --json             Emit the schema v1 team envelope
  -h, --help             Print help
```

## `knobyte log`

Append a note, decision, discovery, risk, or todo to the event log

```text
Usage: knobyte log [OPTIONS] <MESSAGE>

Arguments:
  <MESSAGE>  

Options:
      --kind <KIND>      Event kind: decision, discovery, note, risk, todo [default: note]
      --tag <TAGS>       
      --file <FILES>     Related file path (repeatable)
      --source <SOURCE>  Where the event came from (e.g. meeting, manual, agent)
      --status <STATUS>  Lifecycle status (e.g. decided, implemented)
  -h, --help             Print help
```

## `knobyte timeline`

Search recent event log entries and project notes

```text
Usage: knobyte timeline [OPTIONS]

Options:
      --query <QUERY>    Case-insensitive text in the summary, tags or details
      --kind <KIND>      Filter by event kind: decision, discovery, note, risk, todo
      --file <FILES>     Exact recorded file path (project-relative); any may match (repeatable, max 16)
      --since <SINCE>    From YYYY-MM-DD, RFC 3339, or relative Nd such as 30d
      --limit <LIMIT>    Maximum entries, 1-200 [default: 20]
      --json             
      --format <FORMAT>  Output format: md for a Markdown table
  -h, --help             Print help
```

## `knobyte heartbeat`

Health check: stale content docs, workstream consistency, temp-file cleanup

```text
Usage: knobyte heartbeat [OPTIONS]

Options:
      --json                     
      --clean                    Remove orphaned temporary files and stale locks (otherwise only reported)
      --stale-days <STALE_DAYS>  Days since `last_updated` after which scaffold files are stale (default: heartbeat.staleDays, else 7)
  -h, --help                     Print help
```

## `knobyte doctor`

Comprehensive health diagnostic summary

```text
Usage: knobyte doctor [OPTIONS]

Options:
      --json  
  -h, --help  Print help
```

## `knobyte skills`

Sync official agent skills for Claude Code and Codex

```text
Usage: knobyte skills <COMMAND>

Commands:
  sync  Install or update the official skills and managed instruction blocks (never clobbers edits)

Options:
  -h, --help  Print help
```

### `knobyte skills sync`

Install or update the official skills and managed instruction blocks (never clobbers edits)

```text
Usage: knobyte skills sync [OPTIONS]

Options:
      --tool <TOOL>  claude, codex or all
      --dry-run      
      --backup       Move conflicting skill directories to .knobyte/local/skill-backups/ and reinstall
      --json         
  -h, --help         Print help
```

## `knobyte capabilities`

Structured capability discovery for AI agents

```text
Usage: knobyte capabilities [OPTIONS]

Options:
      --json  
  -h, --help  Print help
```

## `knobyte mcp`

Start the Model Context Protocol (MCP) server (HTTP: streamable /mcp and legacy /sse)

```text
Usage: knobyte mcp [OPTIONS]

Options:
      --port <PORT>        HTTP port (1-65535) [default: 3005]
      --host <HOST>        HTTP bind address (non-loopback requires a bearer token) [default: 127.0.0.1]
      --sse                Serve only the legacy HTTP+SSE transport (GET /sse + POST /messages)
      --http               Serve only the streamable HTTP transport (POST /mcp)
      --stdio              Speak MCP over stdin/stdout instead of HTTP (for clients that spawn the server)
      --token <TOKEN>      Bearer token required by the MCP server (exported as KNOBYTE_MCP_TOKEN)
      --profile <PROFILE>  Tool profile to list (default core; overrides KNOBYTE_MCP_PROFILE and mcp.profile in .knobyte/config.json) [possible values: core, team, wiki, graph, full]
  -h, --help               Print help
```

## `knobyte pattern`

Create a new pattern template

```text
Usage: knobyte pattern <COMMAND>

Commands:
  add   Create patterns/<name>.md (never overwrites) and list it in patterns/INDEX.md

Options:
  -h, --help  Print help
```

### `knobyte pattern add`

Create patterns/<name>.md (never overwrites) and list it in patterns/INDEX.md

```text
Usage: knobyte pattern add <NAME>

Arguments:
  <NAME>  

Options:
  -h, --help  Print help
```

## `knobyte logging`

Read or set this checkout's advisory agent logging mode

```text
Usage: knobyte logging [OPTIONS] [MODE]

Arguments:
  [MODE]  significant (default), checkpoints or manual

Options:
      --expected-revision <EXPECTED_REVISION>
          Require the exact current revision (sha256:...), or `none` for an unset preference
      --json
          
  -h, --help
          Print help
```

## `knobyte watch`

Install/uninstall a post-commit drift check, or run the heartbeat on an interval

```text
Usage: knobyte watch [OPTIONS]

Options:
      --uninstall             Remove the post-commit hook
      --interval [<MINUTES>]  Run `knobyte heartbeat` every N minutes instead (default: watch.intervalMinutes, else 30)
  -h, --help                  Print help
```

## `knobyte completion`

Print a shell completion script (bash, zsh or fish)

```text
Usage: knobyte completion <SHELL>

Arguments:
  <SHELL>  

Options:
  -h, --help  Print help
```

## `knobyte tui`

Interactive terminal dashboard

```text
Usage: knobyte tui

Options:
  -h, --help  Print help
```

## `knobyte commands`

Print list of all available commands

```text
Usage: knobyte commands

Options:
  -h, --help  Print help
```
