# Knobyte Inbox CLI workflows

## Legacy Markdown edit (append to a scaffold document)

```
knobyte inbox draft save --title "<title>" --target "context/<doc>.md" \
  --content "<markdown>" --reason "<why>" [--mode append|replace]
```

`--target` is a Markdown path relative to `.knobyte/`. `--mode append` (default) appends to the document; `--mode replace` replaces it.

## Typed knowledge change

```
knobyte inbox target <entity-id> --json          # exact revision for an update
knobyte inbox draft save --change knowledge.update --entity <entity-id> \
  --target-revision <sha256:...> --body "<replacement body>" --reason "<why>" --preview --json
knobyte inbox draft save --apply '<envelope>' --json
```

## Request files

For long bodies, write a request JSON file that follows `knobyte inbox contract --json` and pass it with `--request <file>`. Add `--operation-id <id>` to make replays idempotent.

## Review lifecycle (human-only)

```
knobyte inbox draft list
knobyte inbox publish <draft-id>          # draft -> pending proposal
knobyte inbox proposal list
knobyte inbox proposal show <proposal-id>
knobyte inbox approve <proposal-id> [--note "..."]
knobyte inbox reject <proposal-id> [--note "..."]
```

## Exit codes

`0` ok, `1` validation, `2` usage, `3` unavailable, `4` conflict (revision changed: re-read and retry), `5` refused.
