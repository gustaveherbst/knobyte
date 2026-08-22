---
name: knobyte-relay
description: Package context, progress, and next actions as a durable handoff in Knobyte. Use when ending a session or passing work to another engineer or agent.
---

# Knobyte Relay

Use this skill when completing a session or passing context to another engineer or agent. A relay draft is checkout-local until a human publishes it.

## Workflow

1. Save a local draft (list flags are repeatable):

   `knobyte relay draft save --title "<title>" --summary "<summary>" [--to <member-id>]... [--completed "<done>"]... [--in-progress "<ongoing>"]... [--decision "<decision>"]... [--blocker "<blocker>"]... [--question "<open question>"]... [--next "<next action>"]... [--evidence "<file, test or command>"]...`

   - `--to` addresses named teammates; without it the relay is open to the whole team (`--audience team`).
   - `--sender` defaults to the current member (`knobyte member current`).
   - Changed files are detected from git unless `--no-auto-files`; branch and HEAD are captured on publish.
   - Add `--preview --json` first to see the exact draft, then `--apply '<envelope>'`.
2. Show the draft (`knobyte relay draft list`) and publish only when asked: `knobyte relay publish <draft-id>`.
3. Inspect a relay with `knobyte relay show <relay-id>`.

## Rules

- Acknowledging (`knobyte relay acknowledge <id>`) and closing (`knobyte relay close <id>`) need explicit confirmation from the user.
- After a write, say exactly what changed and its sharing boundary: a draft is local; a published relay is in the working tree and needs commit and push to be shared.
- See `references/cli-workflows.md` or `knobyte relay contract --json` for request files.
