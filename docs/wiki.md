# Wiki

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

The wiki is the project's knowledge as typed entities: architecture, components, decisions,
conventions, patterns, specs and more. Entities are written in Markdown under `.knobyte/`,
indexed into `.knobyte/wiki.db` (SQLite FTS5) and mirrored into CozoDB for vector search. The
Markdown is canonical, and the index can always be rebuilt from it.

---

## Entity model

Every `*.md` / `*.mdx` file under `.knobyte/` is indexed. Excluded are dot-directories,
`local/` and the `wiki.exclude` globs in `config.json` (default `**/node_modules/**`).
A file is one entity, described by YAML frontmatter:

```markdown
---
id: kb_token_validation
type: component
status: in_flight
revision: 1
title: Token validation
summary: Rejects empty and short bearer tokens.
topics: [auth]
aliases: [token check]
relations:
  - type: implements
    target_id: kb_tokens_need_more_than_8_characters
grounds_to:
  - function:src/auth.rs:validate_token
sources:
  - type: commit
    ref: fb83df4
---

# Token validation

`validate_token` rejects tokens of 8 characters or fewer.
```

| Field | Values |
|---|---|
| `type` | architecture, component, decision, convention, pattern, guide, risk, fact, task, topic, spec, requirement, constraint, acceptance_criterion (more can be registered under `wiki.entityTypes`) |
| `status` | `in_flight`, `promoted`, `deprecated`, `archived` (missing means `promoted`; older words such as `accepted` or `draft` are mapped and flagged `LEGACY_LIFECYCLE_STATE`) |
| relation `type` | depends_on, implements, supersedes, contradicts, derived_from, grounded_in, related_to, affects, verified_by, refines, constrained_by, caused_by |
| source `type` | file, symbol, commit, pull_request, issue, document, manual, agent_session, test, url |
| `revision` | integer ≥ 1, bumped by every operation |

More rules:

- **Id:** when there is no `id`, it is derived as `kb_<file stem>`.
- **Type:** when there is no `type`, it is inferred from the path (`specs/` → spec,
  `patterns/` → pattern, `topics/` → topic, `context/` → architecture).
- **Relation shorthand:** a relation type can also be written as a key, for example
  `implements: [kb_x]`.
- **Team records:** members, workstreams, proposals and relays are read as entities but
  never written by wiki operations.

### Several entities in one file

A `<!-- kb:entity -->` marker turns the next heading into its own entity. Its body runs until
the next heading of the same or shallower depth, or the next marker.

```markdown
<!-- kb:entity id=kb_retry_policy type=decision status=promoted -->
## Retry with exponential backoff

<!-- kb:entity
id: kb_retry_limit
type: constraint
topics: [webhooks]
-->
## At most five attempts
```

A marker that is not followed by a heading is reported as `UNBOUND_ENTITY_METADATA`. Two
markers on one heading are `DUPLICATE_ENTITY_METADATA`. Markers inside code fences are
ignored.

### Groundings

`grounds_to` entries and `<!-- kb-ground: ref #hash -->` anchors tie an entity to code. Each
grounding has a health: `fresh`, `unverified`, `ambiguous`, `changed` or `missing`. An entity's
health is its worst grounding, and `--health none` filters entities without groundings. See
[Grounding and drift](grounding-and-drift.md).

---

## Reading

```text
$ knobyte wiki list
[architecture] Architecture (kb_architecture) (in_flight) [fresh] context/architecture.md:1
[convention] Coding Conventions (kb_conventions) (in_flight) [fresh] context/conventions.md:1
[decision] Decisions (kb_decisions) (in_flight) context/decisions.md:1
...

$ knobyte wiki show kb_architecture --no-body
# Architecture (kb_architecture)
Type: architecture | Status: in_flight | Revision: 1 | File: context/architecture.md:1-50
Summary: How the major pieces of demo connect and flow. Load when working on system design, integrations, or how components interact.
  grounded: function:src/auth.rs:validate_token [fresh]
  grounded: method:src/auth.rs:Session::is_valid [fresh]
  grounded: function:src/crypto.rs:verify_password [fresh]

$ knobyte wiki for-code function:src/auth.rs:validate_token
[architecture] Architecture (kb_architecture) (in_flight) [fresh] context/architecture.md:1 via function:src/auth.rs:validate_token [fresh] matched function:src/auth.rs:validate_token
```

| Command | Answers |
|---|---|
| `wiki list` | Entities, filtered by `--type`, `--topic`, `--status` and `--health`. Archived entities are hidden unless `--include-archived`. Paged with `--limit` and `--offset`. |
| `wiki query <text>` | Ranked search: id > title > summary > body. |
| `wiki show <id>` | One entity with its relations, backlinks, groundings and sources. |
| `wiki related <id>` | Bounded neighbourhood (`--depth` up to 5, `--max-tokens`). |
| `wiki backlinks <id>` | Entities that relate to this one. |
| `wiki for-code <ref…>` | Entities grounded in these symbols. |
| `wiki graph [ids…]` | A bounded slice of the entity graph. |
| `wiki trace <id>` | Spec → requirement → decision → component → code → test, with gaps. Tests are callers that live in test files. |

The [Project Hub](hub.md) and the MCP tools `knobyte_wiki_*` give the same views.

---

## Editing with operations

Hand-editing Markdown is fine. Operations add revision checks, scoped writes and an audit
trail. `knobyte wiki apply <file>` takes a JSON object, an array,
`{"operations": [...]}` or JSONL:

```json
[
  {"type": "create-entry",
   "reason": "Document the token rule",
   "payload": {"file": "context/auth.md", "type": "component", "title": "Token validation",
               "summary": "Rejects empty and short bearer tokens.",
               "body": "`validate_token` rejects tokens of 8 characters or fewer.",
               "groundsTo": ["function:src/auth.rs:validate_token"]}},
  {"type": "add-relation", "entityId": "kb_architecture",
   "payload": {"relation": {"type": "depends_on", "target": "kb_token_validation"}}}
]
```

```text
$ knobyte wiki apply ops.json --dry-run
create-entry op_a789f5826a0ab7b8b1baafcb context/auth.md
  kb_token_validation revision 0 -> 1
  --- context/auth.md (new)
  ...
add-relation op_c9222dde0d2ee12e9a735ba6 context/architecture.md
  kb_architecture revision 1 -> 2
  --- context/architecture.md
  @@ -2,10 +2,12 @@
   id: kb_architecture
   type: architecture
   status: promoted
  -revision: 1
  +revision: 2
   title: Architecture
   summary: How the services fit together
   relations:
  +  - type: depends_on
  +    target_id: kb_token_validation
   ---
   # Architecture
   
[ok] Dry run: 2 file(s) would change; nothing written.

$ knobyte wiki apply ops.json
create-entry op_d81b93c5d4e30e13dab194b2 context/auth.md
  kb_token_validation revision 0 -> 1
add-relation op_c9222dde0d2ee12e9a735ba6 context/architecture.md
  kb_architecture revision 1 -> 2
[ok] Applied; 2 file(s) changed.
```

Dry-run diffs are unified diffs (`diff -U3` style): only the lines an operation changes, each
hunk with up to three unchanged lines of context. Frontmatter edits are spliced key by key, so
adding a relation shows the new relation lines and the `revision` bump, not the whole block.

| Operation | Payload |
|---|---|
| `create-entry` | `file`, `type`, `title` (required); `id`, `status` (default `in_flight`), `summary`, `body`, `topics`, `aliases`, `relations`, `sources`, `groundsTo`, `metadata`, `headingDepth`, `insertAt` (`start-of-file`, `end-of-file`, `before-entity`, `after-entity`), `adopt` |
| `update-entry` | any of `title`, `summary` (null clears it), `body`, `appendSources` |
| `set-property` | `property` (type, status, title, summary, topics, metadata, aliases; `id` only pins a derived id) and `value` |
| `add-relation` / `remove-relation` | `relation: {type, target, note?, waived?}` / `type`, `target` |
| `add-source` / `remove-source` | `source` / `sourceIdentity` (`type\|repository\|ref`) |
| `set-grounding` | `groundsTo` (strings or `{ref, body_hash, fingerprint}`), `updateAnchors`, `keepAnchors`, `normalizeAnchors` |
| `supersede-entry` | `replacementId` or a `replacement` create payload, plus `note`. The old entity becomes `deprecated`. |
| `move-entry` | `file`, `insertAt` (marker sections only) |
| `archive-entry` | none |

The operation envelope:

- **Fields:** `{type, entityId, payload, reason?, opId?, baseRevision?, baseContentHash?, actor?, timestamp?}`.
- **Preconditions:** a `baseRevision` or `baseContentHash` that no longer matches fails with
  `REVISION_CONFLICT` / `CONTENT_HASH_CONFLICT`.
- **Atomicity:** a batch applies completely or not at all.
- **Scoped writes:** every other entity in a touched file must stay byte-identical, otherwise
  `WRITE_SCOPE_VIOLATION`.
- **Read-only paths:** `team/`, `workstreams/`, `inbox/`, `relays/`, `events/activity/` and any
  `wiki.readOnly` globs are refused.
- **Actor:** the CLI records the current member.
- **Audit log:** each applied operation appends intent and complete lines to
  `.knobyte/events/operations.jsonl`. Replaying a recorded `opId` with the same payload is a
  no-op.

Over MCP, agents use a two-step form. `knobyte_wiki_plan_operation` returns the diffs and a
single-use plan handle, valid for 15 minutes. `knobyte_wiki_apply_operation` applies that handle
after re-checking every precondition.

### Generated views

A region between `<!-- kb:generated:begin type=<entity type> -->` and `<!-- kb:generated:end -->`
holds a table of every entity of that type. Without `type=`, the type is inferred:
`patterns/INDEX.md` and `patterns/README.md` list patterns, and `decisions.md` lists
decisions. `knobyte wiki regenerate-views [--dry-run]` rewrites stale regions only, and
`wiki validate` flags them as `GENERATED_VIEW_DRIFT`.

### Synthesis

Synthesis turns the code graph into wiki knowledge with your agent. Knobyte makes no model
calls itself.

1. `knobyte wiki synthesis build` finds clusters in the code graph and writes an agent playbook
   to `.knobyte/local/synthesis/playbook.md` (`--print` prints it instead).
2. `knobyte wiki synthesis prepare --stage <stage> [--cluster <id>]` prints deterministic
   context and prompts for one stage: `architecture_component`, `pattern`, `convention`,
   `global` or `relationships`.
3. `knobyte wiki synthesis propose <response.json> [--stage <stage>]` validates the agent's
   answer into operation plans. With `--apply` it writes them through the operation engine.
   Units with confidence ≥ 0.7 land as `promoted`, those ≥ 0.4 as `in_flight`, and lower ones
   are rejected.

### Migration

`knobyte wiki migrate` plans the rewrite of older Knobyte wiki formats:

- legacy statuses;
- `type: document` (becomes `guide`);
- derived ids (pinned);
- `node_id:` / `node:` grounding maps (become `{ref, body_hash, fingerprint}`);
- hashed graph ids (become readable refs);
- `grounds:` / `kb-anchor:` anchors (become `kb-ground:`);
- frontmatter `edges: [{target: context/x.md, condition: ...}]` (each edge whose target path is
  a wiki entity becomes a `related_to` relation to that entity's id, the condition kept as its
  `note`; edges to anything else stay under `edges`, where `knobyte check` still reports them
  as `DEAD_EDGE` when the path is gone).

Add `--apply` to write the plan through audited operations. Running it again changes nothing.
The setup templates already write explicit ids, types, lifecycle states and `relations`, so a
fresh setup has nothing to migrate. In `knobyte catch-up`, a migration's operations show as one
`wiki migrate: N changes` line.

---

## Validation

`knobyte wiki validate` checks the Markdown directly and needs no index. It exits non-zero on
any error-severity finding.

```text
$ knobyte wiki validate
info [ORPHANED_ENTITY] Billing (kb_billing) is promoted but relates to nothing and nothing relates to it (context/billing.md:1)
info [REVISION_DIVERGED] Entity kb_architecture was edited by hand since revision 1 was recorded (context/architecture.md:31)
info [REVISION_DIVERGED] Entity kb_conventions was edited by hand since revision 1 was recorded (context/conventions.md:27)
0 error(s), 0 warning(s), 4 info across 12 entities
```

| Area | Codes |
|---|---|
| Ids, types, lifecycle | `INVALID_ENTITY_ID`, `DUPLICATE_ENTITY_ID`, `INVALID_ENTITY_TYPE`, `INVALID_LIFECYCLE_STATE`, `LEGACY_LIFECYCLE_STATE`, `INVALID_REVISION`, `MISSING_ENTITY_TITLE`, `MISSING_REQUIRED_FIELD`, `INVALID_FIELD_TYPE`, `REVISION_DIVERGED` |
| Relations | `INVALID_RELATION_TYPE`, `INVALID_RELATION_TARGET`, `DUPLICATE_RELATION`, `SELF_RELATION`, `SUPERSESSION_CYCLE`, `CONTRADICTORY_ACTIVE_DECISIONS` (waive with `waived: true`), `INACTIVE_RELATION_TARGET`, `ORPHANED_ENTITY` |
| Topics | `UNKNOWN_TOPIC`, `AMBIGUOUS_TOPIC_REFERENCE`, `INVALID_TOPIC_MEMBER`, `TOPIC_CYCLE` |
| Sources | `MALFORMED_SOURCE`, `INVALID_COMMIT_FORMAT`, `DUPLICATE_SOURCE`, `UNRESOLVED_EXTERNAL_SOURCE`, `SOURCE_FILE_MISSING` |
| Groundings | `MALFORMED_GROUNDING`, `GROUNDING_UNVERIFIED`, `GROUNDING_MIXED_SHAPE`, `GROUNDING_UNRESOLVED`, `GROUNDING_STALE`, `GROUNDING_MISSING`, `AMBIGUOUS_GROUNDING`, `GROUNDINGS_UNCHECKED`, `UNBOUND_ANCHOR`, `ANCHOR_GROUNDING_MISMATCH` |
| Operations | `INVALID_OPERATION_ENVELOPE`, `UNKNOWN_OPERATION_TYPE`, `INVALID_OPERATION_PAYLOAD`, `REVISION_CONFLICT`, `CONTENT_HASH_CONFLICT`, `WRITE_SCOPE_VIOLATION`, `MALFORMED_OPERATION_LOG`, `PLAN_HANDLE_INVALID` |
| Index | `WIKI_INDEX_MISSING`, `WIKI_INDEX_REBUILD_REQUIRED`, `WIKI_INDEX_CORRUPT`, `WIKI_INDEX_BUSY`, `OPERATION_INTERRUPTED`, `INDEX_REFRESH_REQUIRED`, `WIKI_CORPUS_LIMIT_EXCEEDED` |
| Parsing | `WIKI_PARSE_ERROR`, `FRONTMATTER_PARSE_ERROR`, `FRONTMATTER_UNTERMINATED`, `UNBOUND_ENTITY_METADATA`, `DUPLICATE_ENTITY_METADATA`, `ENTITY_RANGE_OVERLAP`, `MERGE_CONFLICT_MARKERS`, `PATH_OUTSIDE_SCAFFOLD` |
| Other | `ENTITY_NOT_FOUND`, `GENERATED_VIEW_DRIFT`, `WIKI_MIGRATION_REQUIRED`, `MIGRATION_ABSTAINED`, `AMBIGUOUS_MIGRATION`, `INVALID_REQUEST`, `INVALID_AGENT_RESPONSE` |

---

## JSON envelope

With `--json`, wiki commands answer:

```json
{
  "schemaVersion": 1,
  "ok": true,
  "data": { },
  "diagnostics": [
    { "code": "ORPHANED_ENTITY", "severity": "info", "message": "…",
      "file": "context/billing.md", "line": 1, "entityId": "kb_billing", "remediation": "…" }
  ]
}
```

`ok` is false exactly when a diagnostic has severity `error`. The exit codes are:

| Exit | Meaning |
|---|---|
| 0 | OK |
| 1 | error diagnostics |
| 2 | invalid request or envelope |
| 3 | index missing, corrupt or needing a rebuild |
| 4 | precondition conflict, invalid plan handle, interrupted operation or busy index |
| 5 | write scope violation or path outside the scaffold |

---

## Index maintenance

```text
$ knobyte wiki index status
Wiki index: fresh (revision 185b29ff8ddd, 12 entities)
```

| Command | Purpose |
|---|---|
| `wiki rebuild-index [--incremental]` | Rebuilds `wiki.db` from the Markdown and syncs CozoDB. `--incremental` re-reads only files whose content hash changed. |
| `wiki index status` | `missing`, `fresh`, `stale`, `degraded`, `rebuild_required`, `corrupt` or `migration_required`, with the indexed revision. |
| `wiki index dump [--out FILE]` | A deterministic, normalized dump of every row, without wall-clock values. |
| `wiki index doctor` | An integrity check plus a diff of the live index against a clean rebuild. The live index is not modified. |

---

## Export

`knobyte export` writes every scaffold Markdown file into one bundle that starts with
`# knobyte scaffold export`, one `## <path>` section per file. It writes to stdout, or to a file
with `--out PATH`. `--out` only overwrites a file that is itself a previous bundle.
