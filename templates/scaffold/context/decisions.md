---
id: kb_decisions
title: Decisions
type: decision
summary: Key architectural and technical decisions in {{PROJECT_NAME}} with their reasoning. Load when making design choices or asking why something is built a certain way.
status: in_flight
revision: 1
triggers:
  - "why do we"
  - "why is it"
  - "decision"
  - "alternative"
  - "we chose"
relations:
  - type: related_to
    target_id: kb_architecture
    note: when a decision relates to system structure
  - type: related_to
    target_id: kb_stack
    note: when a decision relates to technology choice
# Decisions usually ground sparsely; add only symbols that implement the decision.
grounds_to: []
last_updated: {{TODAY}}
---

<!-- knobyte:populate -->
# Decisions

<!-- When a decision changes, do NOT delete the old entry: mark it superseded and add the new
     entry above it. The history is the record of why the code looks the way it does. -->

## Decision Log

<!-- Record non-obvious decisions where the "why" prevents future mistakes. At least 3 during
     initial population (use "[TO DETERMINE]" entries for pending ones).

     ### <Decision title>
     **Date:** YYYY-MM-DD (from git history when possible)
     **Status:** Active | Superseded by <title>
     **Decision:** what was decided, in one sentence
     **Reasoning:** why
     **Alternatives considered:** what else, and why it was rejected
     **Consequences:** what this means for the codebase going forward -->
