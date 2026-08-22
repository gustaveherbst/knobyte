---
name: knobyte-inbox
description: Propose an addition or correction to project knowledge in Knobyte. Use when a discovery, decision, or correction should become shared project memory.
---

# Knobyte Inbox

Use this skill when you make a discovery, settle an architecture decision, or need to correct existing project knowledge. Inbox drafts are checkout-local until a human publishes them; approval is always a human review decision.

## Workflow

1. Decide what kind of change it is:
   - A new knowledge entity: `--change knowledge.create --kind <architecture|component|convention|decision|pattern|guide>`
   - A correction to an existing entity: resolve it first with `knobyte inbox target <id> --json`, then `--change knowledge.update --entity <id> --target-revision <sha256:...>`
   - A requirement or spec: `--change spec.create` / `spec.update`
2. Preview the exact change without writing anything:

   `knobyte inbox draft save --change knowledge.create --kind decision --title "<title>" --body "<markdown>" --reason "<why this matters>" --evidence "code:<symbol>" --preview --json`

3. Apply the exact preview envelope (a checkout-local draft; nothing is shared):

   `knobyte inbox draft save --apply '<envelope from step 2>' --json`

4. Show the draft (`knobyte inbox draft list`). Publish only when the user asks: `knobyte inbox publish <draft-id>`.

## Rules

- `--reason` is required; cite evidence (`entity:<id>`, `code:<symbol>`, `commit:<hash>`, `file:<path>`, a URL, or a note).
- Never approve, reject or withdraw a proposal yourself. `knobyte inbox approve|reject <id>` are human decisions.
- After a write, say exactly what changed and its sharing boundary: a draft is local to this checkout; a published proposal is written to the working tree and needs commit and push to be shared.
- For the full request-file contract, see `references/cli-workflows.md` or `knobyte inbox contract --json`.
