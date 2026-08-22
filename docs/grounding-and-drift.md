# Grounding and Drift

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

Knowledge goes stale when the code it describes changes. Knobyte ties a claim to the symbols
that implement it (*grounding*). It then reports drift: claims whose code changed, moved or
disappeared, and scaffold content that no longer matches the repository.

---

## Grounding references

A grounding is a readable reference of the form `kind:path:qualified_name`, for example
`function:src/auth.rs:validate_token` or `method:src/auth.rs:Session::is_valid`. Copy these from
graph output: `knobyte graph scope`, `graph query` and `graph get` all print them. You can write
a grounding in either of two places.

**Frontmatter `grounds_to`.** Each entry is a plain reference or a committed baseline:

```yaml
grounds_to:
  - function:src/auth.rs:login                      # no baseline yet
  - ref: function:src/auth.rs:validate_token        # committed baseline
    body_hash: a7813b11c6d6…                         # SHA-256 of the normalized body
    fingerprint: mh1:20:05b1fbf5…                    # MinHash sketch used to find moves
```

**Inline anchors.** An anchor sits on its own line in the body, next to the prose it supports.
The optional `#<body_hash>` is the anchor's baseline:

```markdown
<!-- kb-ground: function:src/crypto.rs:verify_password #94689e58f48a… -->
```

Older anchor spellings, `<!-- grounds: … -->` and `<!-- kb-anchor: … -->`, are still read.
`knobyte wiki migrate` rewrites them to `kb-ground`.

### Baselines are committed

Body hashes and fingerprints live **in the Markdown**, so every clone checks against the same
baseline. A cache in `graph.db` is only a fallback. A grounding without a committed baseline is
reported as `GROUNDING_UNVERIFIED`, because body drift cannot be detected without one.

You create or accept baselines with `knobyte graph ground`:

| Command | Effect |
|---|---|
| `knobyte graph ground` / `--rebaseline` | Records the current body hash and fingerprint for every grounded reference. Run it after reviewing a change. |
| `knobyte graph ground --dry-run` | Proposes `grounds_to` entries for wiki entities that have none, from graph evidence (`--per-entity`, default 3). Writes nothing. |
| `knobyte graph ground --apply` | Writes those proposals into the documents, then re-baselines. |
| `knobyte graph ground --agent --dry-run` | Prints the prompt for agent-led retro-grounding. |
| `knobyte graph ground --agent` | Shows the Claude Code or Codex command and asks before launching it with that prompt, then re-baselines. Add `--launch-agent` to launch without asking; `KNOBYTE_NO_AGENT_LAUNCH=1` prevents the launch. |

`knobyte setup` captures baselines when it finishes a populated scaffold. `knobyte sync --accept`
captures them for files an agent has just repaired.

---

## `knobyte check`

Without `--fix`, `knobyte check` is **read-only**. It scans the scaffold and the repository and
prints issues by severity, then a score:

```text
$ knobyte check
ERROR

.knobyte/context/conventions.md
  x GROUNDING_GONE Grounded node no longer exists: function:src/auth.rs:logout
    -> Update the prose and remove or replace the grounding.

INFO

.knobyte/context/conventions.md
  i GROUNDING_UNVERIFIED Grounded node has no committed baseline, so body drift cannot be detected: function:src/auth.rs:login. Run `knobyte graph ground --rebaseline` to record it.
    -> Stale graph: run `knobyte graph refresh`. No baseline yet: run `knobyte graph ground --rebaseline`. Then check again.

Drift score: 89/100 — 1 errors, 0 warnings, 1 info
12 files checked
Groundings: 60.0% intact (3 intact, 0 changed, 0 moved, 0 ambiguous, 1 gone, 1 unverified of 5)
graph fresh
```

- **Score:** `100 − 10 × errors − 3 × warnings − 1 × info`, clamped to 0-100.
  `GROUNDING_MOVED_BY_NEIGHBORS` is not scored.
- **Grounding percentage:** reported separately, as intact groundings ÷ all groundings
  (`grounding_score` in `--json`).
- **Exit code:** 1 when there is at least one error, otherwise 0, so it can gate CI or a hook.
- **Output modes:** `--quiet` prints one line (`knobyte: drift score 100/100 · graph fresh`),
  `--json` prints the full report and `--verbose` adds per-checker detail.
- **`--fix`:** writes, so it plans first. It prints every planned rewrite (file, old reference
  -> new reference, confidence), then:
  - in an interactive terminal it asks `Apply these N grounding change(s)? [Y/n]`;
  - with `--yes` it applies without asking;
  - in a non-interactive session without `--yes` it writes nothing and exits 5 with a hint;
  - with `--json` it never asks: the plan is in the report's `fix` object
    (`{dryRun, applied, planned: [{file, oldRef, newRef, confidence, reason}]}`), and it writes
    only with `--yes` (exit 5 otherwise);
  - with `--dry-run` it prints the plan only and exits 0.

  If errors remain after the rewrites, it continues into the `sync` repair flow below, which
  asks before launching an agent. Declining the rewrites skips that flow.

```text
$ knobyte check --fix
Planned grounding changes (1):
  context/billing.md
    function:src/old.rs:calculate_tax -> function:src/new.rs:calculate_tax  (100% confidence)

Apply these 1 grounding change(s)? [Y/n]
```

### Checker codes

| Code | Severity | Meaning |
|---|---|---|
| `GROUNDING_DRIFT` | warning | The grounded body changed since its baseline, or the symbol moved (an anchor that should be rewritten) |
| `GROUNDING_GONE` | error (frontmatter), warning (inline anchor) | The grounded symbol no longer exists |
| `GROUNDING_AMBIGUOUS` | warning | Several symbols match, committed fingerprints conflict, or a move has several candidates |
| `GROUNDING_UNVERIFIED` | info without a committed baseline, warning when the graph is stale | Drift cannot be decided |
| `GROUNDING_MOVED_BY_NEIGHBORS` | info, unscored | A rename or move decided from caller and callee continuity |
| `GROUNDING_MIXED_SHAPE` | info, or warning when mapping fields contradict the reference | Inconsistent `grounds_to` shapes |
| `STALE_FILE` | warning or error | Too many days or commits since the file last changed, or an old `last_updated` |
| `MISSING_PATH` | error (warning for pattern or placeholder paths) | A referenced path does not exist |
| `DEAD_COMMAND` | error | A documented command or script no longer exists (npm/yarn/pnpm/bun scripts, make targets, `swift run` executables) |
| `DEPENDENCY_MISSING` | warning | A claimed dependency is not in the manifest (`package.json`, `pyproject.toml`, `Cargo.toml`, `Package.swift` packages and products) |
| `VERSION_MISMATCH` | warning | A claimed version differs from the manifest |
| `CROSS_FILE_CONFLICT` | error (conflicting versions), warning (mixed package managers) | Scaffold files contradict each other |
| `DEAD_EDGE` | error | A frontmatter `edges[].target` or `relations[].target_id` does not exist |
| `INDEX_MISSING_ENTRY`, `INDEX_ORPHAN_ENTRY` | warning | `patterns/INDEX.md` and the pattern files disagree |
| `UNDOCUMENTED_SCRIPT` | warning | A `package.json` script is never mentioned |
| `TOOL_CONFIG_DRIFT` | warning | Agent tool-config copies differ (managed blocks excluded) |
| `TODO_FIXME` | warning | TODO or FIXME left in scaffold Markdown |
| `BROKEN_LINK` | error (warning under `patterns/`) | A Markdown link does not resolve |
| `MISSING_FRONTMATTER_FIELD` | warning | Recommended frontmatter fields are missing in `context/` or `patterns/` |
| `FRONTMATTER_PARSE_ERROR`, `FRONTMATTER_UNTERMINATED` | error | A scaffold file's frontmatter is invalid YAML (for example a duplicate `grounds_to` key) or never closed, so every field in it is ignored; reported with the offending line instead of the resulting missing-field warnings |
| `STALE_PATTERN` | warning | A pattern is unreachable from `ROUTER.md` or `context/*.md` |
| `SCAFFOLD_ORPHANED` | error | A populated scaffold that no always-loaded agent file points to |
| `UNREADABLE_FILE` | error | A scaffold file could not be read |
| `SCAFFOLD_MISSING` | error | There is no scaffold directory |

The staleness thresholds default to 30 and 90 days, and 50 and 200 commits. Override them per run
with `--stale-warn-days`, `--stale-error-days`, `--stale-warn-commits` and
`--stale-error-commits`, or in `.knobyte/config.json` under `staleness_thresholds`
(`warn_days`, `error_days`, `warn_commits`, `error_commits`).

---

## How moves are found

When a grounded reference no longer resolves, Knobyte looks for the symbol in this order:

1. an exact body-hash match;
2. the same readable name;
3. MinHash similarity combined with caller and callee continuity, which decides MOVED,
   AMBIGUOUS or GONE.

Only high-confidence moves are rewritten automatically.

```text
$ knobyte sync --dry-run
=== Grounding Anchor Relocations ===
  - [context/architecture.md] verify_password: src/password.rs -> src/crypto.rs (confidence: 100%)
[info] 1 anchor(s) eligible for relocation (dry run).
```

---

## `knobyte sync`: repair drift

`knobyte sync` runs in three stages:

1. **Relocation.** It rewrites high-confidence moves in `grounds_to` entries and
   `<!-- kb-ground -->` anchors. References that still resolve are not touched. This stage is
   skipped when the graph is not fresh; run `knobyte graph refresh` first.
2. **Repair brief.** It re-checks and lists the files that still have errors (`--warnings` adds
   warning-only files). For each file it builds one prompt with the issues, the file content, and
   the old and new body of every changed grounded symbol.
3. **Repair session.** It hands that prompt to an agent:
   - `--dry-run` prints the prompt and exits.
   - `--print-prompt` prints it and exits 1 if errors remain.
   - Otherwise sync shows the launch command and asks: launch the agent, show the prompt, or exit.
     `--launch-agent` skips that question. `--agent claude|codex` chooses the agent.
   - With `KNOBYTE_NO_AGENT_LAUNCH=1`, in CI or in a non-interactive shell, nothing is launched
     and the prompt is printed instead.

Sync runs up to `--max-cycles` (default 3) repair cycles. After each cycle it prints the score
change (`Drift score: A -> B/100`). When repairs complete, `--accept`, or a yes to the
confirmation, captures grounding baselines for the repaired files. The agent itself is told
never to accept a changed body, because that is your decision.

Exit codes: 0 when no errors remain, 1 when issues remain, 130 when interrupted.

---

## Automating the check

```bash
knobyte watch                 # install a post-commit hook that runs `knobyte check --quiet`
knobyte watch --uninstall
knobyte watch --interval 30   # instead: run `knobyte heartbeat` every 30 minutes in the foreground
```

The hook section sits between `# knobyte-drift-check` and `# knobyte-drift-check:end`. It is
appended to an existing `post-commit` hook and reports only when the score is below 100.

In CI, `knobyte check` (exit 1 on errors) or `knobyte check --json` is enough.

---

## Related

- `knobyte wiki validate` checks the wiki's own structure, including grounding shape. See
  [Wiki](wiki.md).
- The Hub's **Groundings** page shows the same issues with a relocation preview and an Apply
  button. Each Knowledge page has a drift panel comparing the committed baseline with the current
  source. See [Project Hub](hub.md).
