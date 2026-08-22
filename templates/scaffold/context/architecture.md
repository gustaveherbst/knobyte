---
id: kb_architecture
title: Architecture
type: architecture
summary: How the major pieces of {{PROJECT_NAME}} connect and flow. Load when working on system design, integrations, or how components interact.
status: in_flight
revision: 1
triggers:
  - "architecture"
  - "system design"
  - "how does X connect to Y"
  - "integration"
  - "flow"
relations:
  - type: related_to
    target_id: kb_stack
    note: when specific technology details are needed
  - type: related_to
    target_id: kb_decisions
    note: when understanding why the architecture is structured this way
# Broad overview: keep this empty unless a claim depends on a few specific symbols.
# Entry shape: "function:<path>:<qualified_name>" taken from `knobyte graph scope` output.
grounds_to: []
last_updated: {{TODAY}}
---

<!-- knobyte:populate -->
# Architecture

## System Overview
<!-- How the major pieces connect. Focus on FLOW, not technology: how does a request or action
     move through the system? Use the real component and module names.
     5-15 lines, readable in 30 seconds. -->

## Key Components
<!-- The major components, modules, or services: name, what it does, what it depends on.
     At least 3. Write "[TO DETERMINE]" when you cannot identify 3. 1-2 lines each. -->

## External Dependencies
<!-- Third-party services, APIs, or databases this project talks to and any constraints.
     At least 3, or "[TO DETERMINE]". 1-2 lines each. -->

## What Does NOT Exist Here
<!-- Explicit boundaries: what is deliberately outside this system. 2-5 items. -->
