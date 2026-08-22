---
id: kb_heartbeat
name: heartbeat
description: Lightweight checks for scheduled persistent-agent heartbeat events.
type: guide
status: promoted
last_updated: {{TODAY}}
---

# Heartbeat

Run these checks when the agent receives a heartbeat event.

## Checks

1. Run `knobyte heartbeat`.
2. If it prints `HEARTBEAT_OK`, respond with exactly `HEARTBEAT_OK`.
3. If it reports stale scaffold files, tell the user which files need review and suggest `knobyte sync`.
4. If memory cleanup is due, review the dated daily memory files, promote durable insights to the long-term memory file, and record the cleanup date.
5. Do not perform unrelated work during a heartbeat.

## Defaults

- Context staleness: 7 days since `last_updated`, unless config.json sets `heartbeat.staleDays`.
- Memory cleanup: due 7 days after the last cleanup, unless config.json sets `heartbeat.memoryCleanupDays`.
- Daily memory retention: 14 days, unless config.json sets `heartbeat.dailyMemoryRetentionDays`.
