# Team Memory & Collaboration Workflows

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

Software engineering is rarely a solo activity, and AI agents often operate asynchronously across different pull requests and sessions. Knobyte provides built-in contracts for **Relays** (handoffs), **Inbox** (proposals), **Members**, and **Timeline logging**.

---

## 1. Relays (Schema v4 Handoffs)

A **Relay** transfers complete context between developers and AI agents when ending a session, pausing work, or transitioning tasks.

### Relay Structure
- **Title & Summary**: High-level intent of the work.
- **Sender & Recipients**: Attributed author and target team members.
- **Progress**: Bulleted list of completed items.
- **Blockers**: Active technical hurdles or questions.
- **Next Actions**: Concrete next steps for the next engineer or agent.
- **Observed State**: Repository branch, HEAD commit, and dirty tree status at handoff time.

### CLI Workflow

```bash
# 1. Draft a handoff
knobyte relay draft save

# 2. Publish to the project
knobyte relay publish <draft_id>

# 3. Teammate or agent claims the relay
knobyte relay acknowledge <relay_id> --member mem_sam

# 4. Once finished, close the relay
knobyte relay close <relay_id> --member mem_sam
```

---

## 2. Knowledge Inbox (Proposals)

When an agent or developer discovers an architectural change, edge case, or pattern, they should not edit authoritative documentation unchecked. Instead, they propose a change through the **Inbox**.

### Inbox Lifecycle
1. **Draft**: Create a proposal in `.knobyte/local/inbox_drafts/`.
2. **Publish**: Move to `.knobyte/inbox/` with status `pending`.
3. **Review**: Team reviews via CLI or Project Hub before integrating into `.knobyte/context/`.

```bash
# Save an inbox draft
knobyte inbox draft save

# Publish proposal for review
knobyte inbox publish <draft_id>

# View pending proposals
knobyte inbox proposals
```

---

## 3. Team Member Attribution

Knobyte supports canonical team member profiles in `.knobyte/team/members/` with local session overrides:

- **Canonical Identity**: `id`, `display_name`, `git_aliases` (author name & email).
- **Active Session Selection**: Stored locally in `.knobyte/local/current_member.json` so actions taken on a developer's machine are automatically attributed to them.

```bash
# List team members
knobyte member list

# Select active identity
knobyte member select mem_alex

# Inspect current contributor
knobyte member current
```

---

## 4. Timeline & Decision Logging

All architectural decisions, discoveries, and risks are appended to `.knobyte/events/decisions.jsonl`.

```bash
# Record an architecture decision
knobyte log "Adopted CozoDB Sled storage for persistent HNSW vectors" --kind decision --tags "database,vectors"

# Search historical events
knobyte timeline --kind decision --query "database"
```
