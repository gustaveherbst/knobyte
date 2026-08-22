# Knobyte Relay CLI workflows

## Draft, preview, apply

```
knobyte relay draft save --title "<title>" --summary "<summary>" \
  --completed "<done>" --next "<next action>" --preview --json
knobyte relay draft save --apply '<envelope>' --json
knobyte relay draft list
```

## Sparse JSON drafts

`knobyte relay draft save --from <draft.json>` creates a draft from the fields of the relay draft contract (`knobyte relay contract --json`). `--request <file>` takes a complete caller-authored request.

## Lifecycle (needs user confirmation)

```
knobyte relay publish <draft-id>      # captures branch, HEAD and dirty-tree state
knobyte relay list
knobyte relay show <relay-id>
knobyte relay acknowledge <relay-id>  # claim it
knobyte relay close <relay-id>        # sender or claimant
```

## Exit codes

`0` ok, `1` validation, `2` usage, `3` unavailable, `4` conflict, `5` refused.
