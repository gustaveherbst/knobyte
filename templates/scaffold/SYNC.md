# Keeping the Scaffold in Sync

The scaffold drifts as the code changes. Knobyte detects that drift deterministically and repairs only what is broken.

## Detect

`knobyte check` scores the scaffold (100 minus 10 per error, 3 per warning, 1 per info) against the code graph, the filesystem, manifests and git history. `knobyte check --quiet` prints one line; `knobyte watch` runs it after every commit.

## Repair

`knobyte sync` runs the repair loop:

1. Relocate grounding anchors whose symbols moved.
2. Build a repair brief for every file with errors (and every grounding issue).
3. Launch Claude Code or Codex with the brief after you confirm, or print the prompt to paste into your agent.
4. Re-run the drift check and show the score change.
5. Capture grounding baselines for the repaired files once you accept the result.

`knobyte sync --dry-run` prints the brief without launching anything; `--warnings` includes warning-only files.

## Rules for the repairing agent

- Fix only the reported issues; preserve verified prose and authored decisions.
- Re-verify each changed claim against the code graph (`knobyte graph scope`, `knobyte graph get --source`).
- Bump `last_updated` on every file you change.
