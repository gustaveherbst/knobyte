---
id: kb_stack
title: Technology Stack
type: architecture
summary: Technology stack, library choices, and the reasoning behind them for {{PROJECT_NAME}}. Load when working with specific technologies or choosing libraries.
status: in_flight
revision: 1
triggers:
  - "library"
  - "package"
  - "dependency"
  - "which tool"
  - "technology"
relations:
  - type: related_to
    target_id: kb_decisions
    note: when the reasoning behind a tech choice is needed
  - type: related_to
    target_id: kb_conventions
    note: when understanding how to use a technology in this codebase
# Broad inventory: ground only claims embodied by a small number of symbols.
grounds_to: []
last_updated: {{TODAY}}
---

<!-- knobyte:populate -->
# Technology Stack

## Core Technologies
<!-- Primary language, framework, and runtime, with versions where they matter. 3-7 items. -->
{{DETECTED_STACK}}
## Key Libraries
<!-- Libraries central to how this project works: "we use THIS, not the alternative", with the
     reason where it matters. 3-10 items, or "[TO DETERMINE]". -->

## What We Deliberately Do NOT Use
<!-- Technologies or patterns explicitly avoided, and why. 2-5 items. -->

## Version Constraints
<!-- Only important version-specific facts; leave empty when there are none. -->
