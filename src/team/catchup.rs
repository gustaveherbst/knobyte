//! Catch Up: a per-member digest of what changed in shared project memory since
//! that member last caught up.
//!
//! The digest aggregates canonical sources only — relays, inbox proposals, the
//! decision/event log, the wiki operation audit log and activity records — and
//! groups them: `handoffs` (relays addressed to me), `reviews` (proposals awaiting
//! my review), `decisions`, `knowledge` (wiki and proposal outcomes),
//! `workstreams`, `playbooks` and other `activity`.
//!
//! The baseline is a checkout-local cursor per actor, stored in
//! `.knobyte/local/catch-up/<actor-key>.json` (never committed). Moving it is a
//! local team workflow action (`catchup.mark` / `catchup.reset`): previewable,
//! locked and journaled like every other team mutation, but it writes nothing
//! shared and records no activity. A cursor belongs to the branch it was marked
//! on; marking on another branch requires an explicit reset.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::KnobyteConfig;
use crate::team::activity::parse_since;
use crate::team::envelope::{paginate, Diagnostic, TeamError};
use crate::team::identity::{resolve_actor, ActorRef};
use crate::team::relay::{observe_repo_state, ObservedRepoState};
use crate::team::store::short_hash;
use crate::team::workflow::{parse_action, read_json_file, Ctx, Plan};

pub const CATCH_UP_SCHEMA_VERSION: u32 = 1;
/// Digest groups, in presentation order.
pub const CATCH_UP_GROUPS: &[&str] = &["handoffs", "reviews", "decisions", "knowledge", "workstreams", "playbooks", "activity"];
/// Baseline used when the actor has no cursor and no `--since` was given.
pub const DEFAULT_WINDOW_DAYS: i64 = 7;
/// Upper bound of items gathered per source before paging.
const MAX_PER_SOURCE: usize = 500;

/// The checkout-local catch-up position of one actor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatchUpCursor {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub actor: ActorRef,
    #[serde(rename = "actorId")]
    pub actor_id: String,
    /// Everything at or before this instant counts as seen.
    pub timestamp: String,
    /// Branch observed when the cursor was marked or reset.
    pub branch: Option<String>,
    pub head: Option<String>,
    #[serde(rename = "markedAt")]
    pub marked_at: String,
}

/// One digest entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatchUpItem {
    pub id: String,
    pub group: String,
    /// `relay`, `proposal`, `log`, `wiki` or `activity`.
    pub source: String,
    #[serde(rename = "occurredAt")]
    pub occurred_at: String,
    pub title: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Entity references (`{kind, id, title?}`).
    pub subjects: Vec<Value>,
    /// True when the item changed after the baseline; false for items that still
    /// need attention (an unacknowledged handoff, a pending review) but are older.
    #[serde(rename = "new")]
    pub is_new: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workstream: Option<String>,
}

/// Digest request.
#[derive(Debug, Clone, Default)]
pub struct CatchUpRequest {
    /// Override the baseline: RFC 3339, `YYYY-MM-DD` or relative `Nd`/`Nh`.
    pub since: Option<String>,
    /// Only items related to this workstream.
    pub workstream: Option<String>,
    /// Also list the actor's own changes.
    pub include_mine: bool,
    /// Only these groups.
    pub groups: Vec<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

pub fn cursor_dir(config: &KnobyteConfig) -> std::path::PathBuf {
    config.local_dir().join("catch-up")
}

/// File-name key of an actor's cursor. Git identity data is hashed so it never
/// appears in a file name.
pub fn actor_key(actor: &ActorRef) -> Option<String> {
    match actor {
        ActorRef::Member { member_id, .. } => Some(format!("member-{}", member_id)),
        ActorRef::Git { .. } => Some(format!("git-{}", short_hash(&actor.id()))),
        ActorRef::Unknown => None,
    }
}

fn cursor_path(config: &KnobyteConfig, actor: &ActorRef) -> Option<std::path::PathBuf> {
    let key = actor_key(actor)?;
    crate::team::validate_entity_id(&key).ok()?;
    Some(cursor_dir(config).join(format!("{}.json", key)))
}

/// The stored cursor of `actor`, if any.
pub fn get_cursor(config: &KnobyteConfig, actor: &ActorRef) -> Option<CatchUpCursor> {
    read_json_file(&cursor_path(config, actor)?)
}

fn parse_ts(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts).ok().map(|t| t.with_timezone(&chrono::Utc))
}

fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn entity(kind: &str, id: &str, title: Option<&str>) -> Value {
    let mut v = json!({ "kind": kind, "id": id });
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        v["title"] = json!(t);
    }
    v
}

fn excerpt(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

struct Window {
    baseline: chrono::DateTime<chrono::Utc>,
    observed: chrono::DateTime<chrono::Utc>,
}

impl Window {
    fn contains(&self, ts: &str) -> bool {
        parse_ts(ts).map(|t| t > self.baseline && t <= self.observed).unwrap_or(false)
    }
    fn is_new(&self, ts: &str) -> bool {
        parse_ts(ts).map(|t| t > self.baseline).unwrap_or(false)
    }
}

fn group_for_activity(entity_kind: &str, action: &str) -> &'static str {
    match entity_kind {
        "workstream" => "workstreams",
        "playbook" | "playbook_run" => "playbooks",
        _ if matches!(action, "inbox.approve" | "inbox.reject") => "knowledge",
        _ => "activity",
    }
}

/// Build the digest for the resolved actor of this checkout.
pub fn catch_up_digest(config: &KnobyteConfig, req: &CatchUpRequest) -> Result<(Value, Vec<Diagnostic>), TeamError> {
    for g in &req.groups {
        if !CATCH_UP_GROUPS.contains(&g.as_str()) {
            return Err(TeamError::usage(format!("--group must be one of: {}", CATCH_UP_GROUPS.join(", "))));
        }
    }
    if let Some(w) = &req.workstream {
        crate::team::validate_entity_id(w).map_err(TeamError::usage)?;
    }
    let resolution = resolve_actor(config);
    let actor = resolution.actor.clone();
    let me = actor.id();
    let my_member = actor.member_id().map(str::to_string);
    let mut diagnostics = resolution.diagnostics.clone();
    let repo: ObservedRepoState = observe_repo_state(&config.project_root);
    let stored = get_cursor(config, &actor);
    let observed = chrono::Utc::now();

    let (baseline, baseline_source) = match (&req.since, &stored) {
        (Some(s), _) => (parse_since(s)?, "since"),
        (None, Some(c)) => match parse_ts(&c.timestamp) {
            Some(t) => (t, "cursor"),
            None => {
                diagnostics.push(Diagnostic::warning("CATCH_UP_CURSOR_INVALID", "The stored catch-up cursor has an invalid timestamp; using the default window. Run `knobyte catch-up reset`."));
                (observed - chrono::Duration::days(DEFAULT_WINDOW_DAYS), "default")
            }
        },
        (None, None) => (observed - chrono::Duration::days(DEFAULT_WINDOW_DAYS), "default"),
    };
    if let Some(c) = &stored {
        if c.branch != repo.branch && req.since.is_none() {
            diagnostics.push(Diagnostic::warning(
                "CATCH_UP_BRANCH_CHANGED",
                format!(
                    "The catch-up cursor was marked on {} but this checkout is on {}; run `knobyte catch-up reset` to adopt this branch.",
                    c.branch.as_deref().map(|b| format!("branch '{}'", b)).unwrap_or_else(|| "a detached HEAD".to_string()),
                    repo.branch.as_deref().map(|b| format!("branch '{}'", b)).unwrap_or_else(|| "a detached HEAD".to_string()),
                ),
            ));
        }
    }
    if matches!(actor, ActorRef::Unknown) {
        diagnostics.push(Diagnostic::warning(
            "CATCH_UP_UNKNOWN_ACTOR",
            "No member is selected and Git has no identity: handoffs and reviews cannot be personalised, and the cursor cannot be marked.",
        ));
    }
    let win = Window { baseline, observed };
    // The event log records the logging label (display name) rather than the id.
    let my_label = my_member.as_deref().and_then(|m| crate::team::members::get_member(config, m)).map(|m| m.display_name);
    let mine = |who: &str| !req.include_mine && (who == me || my_label.as_deref() == Some(who));
    let mut items: Vec<CatchUpItem> = Vec::new();

    // Handoffs: relays addressed to me (or the whole team) that are new, or
    // still waiting for a named recipient's acknowledgement.
    for r in crate::team::relay::list_relays(config).into_iter().take(MAX_PER_SOURCE) {
        if r.sender == me {
            continue;
        }
        let named = my_member.as_deref().map(|m| r.named_recipients.iter().any(|x| x == m)).unwrap_or(false);
        let addressed = named || r.is_team();
        if !addressed {
            continue;
        }
        let changed = win.contains(&r.created_at) || win.contains(&r.updated_at);
        let waiting = named && r.status == "published";
        if !(changed || waiting) {
            continue;
        }
        let ts = if win.contains(&r.updated_at) { r.updated_at.clone() } else { r.created_at.clone() };
        let status = match r.status.as_str() {
            "published" if named => "awaiting your acknowledgement".to_string(),
            "published" => "open to the team".to_string(),
            other => other.to_string(),
        };
        items.push(CatchUpItem {
            id: format!("relay:{}", r.id),
            group: "handoffs".to_string(),
            source: "relay".to_string(),
            occurred_at: ts.clone(),
            title: r.title.clone(),
            summary: format!("From {} ({}): {}", r.sender, status, excerpt(&r.summary, 240)),
            actor: Some(r.sender.clone()),
            subjects: vec![entity("relay", &r.id, Some(&r.title))],
            is_new: win.is_new(&ts),
            workstream: r.workstream.clone(),
        });
    }

    // Reviews: pending proposals I did not author or repair.
    for p in crate::team::inbox::list_inbox_proposals(config).into_iter().take(MAX_PER_SOURCE) {
        if p.status != "pending" {
            continue;
        }
        if my_member.as_deref().map(|m| p.is_contributor(m)).unwrap_or(false) || p.author == me {
            continue;
        }
        items.push(CatchUpItem {
            id: format!("proposal:{}", p.id),
            group: "reviews".to_string(),
            source: "proposal".to_string(),
            occurred_at: p.updated_at.clone(),
            title: p.title.clone(),
            summary: format!("Proposed by {} ({}): {}", p.author, p.change_kind(), excerpt(&p.reason, 240)),
            actor: Some(p.author.clone()),
            subjects: vec![entity("proposal", &p.id, Some(&p.title))],
            is_new: win.is_new(&p.updated_at),
            workstream: None,
        });
    }

    // Decisions and other log entries.
    let (events, _) = crate::events::read_events_bounded(&config.decisions_log_path());
    for e in events.into_iter().rev().take(MAX_PER_SOURCE) {
        if !win.contains(&e.timestamp) {
            continue;
        }
        if e.actor.as_deref().map(&mine).unwrap_or(false) {
            continue;
        }
        let group = if e.kind == "decision" { "decisions" } else { "activity" };
        items.push(CatchUpItem {
            id: format!("log:{}", e.id),
            group: group.to_string(),
            source: "log".to_string(),
            occurred_at: e.timestamp.clone(),
            title: format!("{}: {}", e.kind, excerpt(&e.summary, 120)),
            summary: e.details.as_deref().map(|d| excerpt(d, 240)).unwrap_or_else(|| excerpt(&e.summary, 240)),
            actor: e.actor.clone(),
            subjects: e.files.iter().take(8).map(|f| json!({ "kind": "file", "path": f })).collect(),
            is_new: true,
            workstream: None,
        });
    }

    // Wiki changes: completed typed operations.
    // `wiki migrate` format rewrites are collapsed into one line: they are mechanical and
    // one migration can touch every entity.
    let (audit, _) = crate::wiki::ops::read_audit_log(&config.scaffold_root);
    let mut migration: Vec<crate::wiki::ops::AuditEntry> = Vec::new();
    for a in audit.into_iter().rev().filter(|a| a.phase == "complete").take(MAX_PER_SOURCE) {
        if !win.contains(&a.timestamp) {
            continue;
        }
        if mine(&a.actor.id) {
            continue;
        }
        if a.actor.kind == "system" && a.actor.id == "migration" {
            migration.push(a);
            continue;
        }
        let ids = if a.created_ids.is_empty() { &a.entity_ids } else { &a.created_ids };
        items.push(CatchUpItem {
            id: format!("wiki:{}", a.op_id),
            group: "knowledge".to_string(),
            source: "wiki".to_string(),
            occurred_at: a.timestamp.clone(),
            title: format!("{} {}", a.op_type, ids.iter().take(3).cloned().collect::<Vec<_>>().join(", ")),
            summary: a.reason.as_deref().map(|r| excerpt(r, 240)).unwrap_or_else(|| format!("{} in {}", a.op_type, a.files.join(", "))),
            actor: Some(a.actor.id.clone()),
            subjects: ids.iter().take(8).map(|id| entity("wiki", id, None)).collect(),
            is_new: true,
            workstream: None,
        });
    }
    if let Some(latest) = migration.first() {
        let mut kinds: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut ids: Vec<String> = Vec::new();
        let mut files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for a in &migration {
            let kind = a
                .reason
                .as_deref()
                .and_then(|r| r.strip_prefix("wiki migrate: "))
                .unwrap_or(a.op_type.as_str())
                .to_string();
            *kinds.entry(kind).or_default() += 1;
            for id in &a.entity_ids {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
            files.extend(a.files.iter().cloned());
        }
        let n = migration.len();
        let breakdown = kinds.iter().map(|(k, c)| format!("{} {}", c, k)).collect::<Vec<_>>().join(", ");
        items.push(CatchUpItem {
            id: format!("wiki:migration:{}", latest.op_id),
            group: "knowledge".to_string(),
            source: "wiki".to_string(),
            occurred_at: latest.timestamp.clone(),
            title: format!("wiki migrate: {} change{}", n, if n == 1 { "" } else { "s" }),
            summary: format!(
                "Format migration of {} entit{} in {} file{} ({})",
                ids.len(),
                if ids.len() == 1 { "y" } else { "ies" },
                files.len(),
                if files.len() == 1 { "" } else { "s" },
                breakdown
            ),
            actor: Some(latest.actor.id.clone()),
            subjects: ids.iter().take(8).map(|id| entity("wiki", id, None)).collect(),
            is_new: true,
            workstream: None,
        });
    }

    // Canonical activity (workstreams, playbooks, review outcomes, everything else).
    // Relay publications and proposal publications are already covered above.
    for a in crate::team::activity::list_activity(config, MAX_PER_SOURCE) {
        if !win.contains(&a.timestamp) {
            continue;
        }
        if mine(&a.actor) {
            continue;
        }
        if matches!(a.action.as_str(), "relay.publish" | "inbox.publish") || a.action.contains(".draft.") || a.action == "member.select" {
            continue;
        }
        let group = group_for_activity(&a.entity_kind, &a.action);
        items.push(CatchUpItem {
            id: format!("activity:{}", a.id),
            group: group.to_string(),
            source: "activity".to_string(),
            occurred_at: a.timestamp.clone(),
            title: if a.entity_title.is_empty() { a.action.clone() } else { a.entity_title.clone() },
            summary: excerpt(&a.summary, 240),
            actor: Some(a.actor.clone()),
            subjects: if a.entity_id.is_empty() { Vec::new() } else { vec![entity(&a.entity_kind, &a.entity_id, Some(&a.entity_title))] },
            is_new: true,
            workstream: a.workstream.clone(),
        });
    }

    if let Some(w) = &req.workstream {
        items.retain(|i| {
            i.workstream.as_deref() == Some(w.as_str())
                || i.subjects.iter().any(|s| s["kind"] == "workstream" && s["id"] == w.as_str())
        });
    }
    if !req.groups.is_empty() {
        items.retain(|i| req.groups.contains(&i.group));
    }
    let order = |g: &str| CATCH_UP_GROUPS.iter().position(|x| *x == g).unwrap_or(usize::MAX);
    items.sort_by(|a, b| {
        order(&a.group)
            .cmp(&order(&b.group))
            .then_with(|| b.occurred_at.cmp(&a.occurred_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut counts = serde_json::Map::new();
    for g in CATCH_UP_GROUPS {
        counts.insert(g.to_string(), json!(items.iter().filter(|i| i.group == *g).count()));
    }
    let attention = items.iter().filter(|i| i.group == "handoffs" || i.group == "reviews").count();
    let baseline_s = rfc3339(baseline);
    let page = paginate(
        items,
        |i| format!("{}@{}", i.id, i.occurred_at),
        &format!("baseline={};ws={:?};mine={};groups={:?}", baseline_s, req.workstream, req.include_mine, req.groups),
        req.cursor.as_deref(),
        req.limit,
    )?;
    let data = json!({
        "actor": actor,
        "actorId": me,
        "baseline": baseline_s,
        "baselineSource": baseline_source,
        "observedAt": rfc3339(observed),
        "cursor": stored,
        "repoState": repo,
        "groups": counts,
        "needsAttention": attention,
        "items": page.items,
        "nextCursor": page.next_cursor,
        "truncated": page.truncated,
        "total": page.total,
        "deterministicRevision": page.deterministic_revision,
    });
    Ok((data, diagnostics))
}

// ---------------------------------------------------------------------------
// Cursor mutations (local workflow actions)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkAction {
    /// Mark everything up to this instant as seen (default: now). Pass the
    /// digest's `observedAt` so items arriving while reading are not skipped.
    #[serde(default)]
    at: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetAction {
    /// New baseline (RFC 3339, `YYYY-MM-DD` or relative `Nd`/`Nh`; default now).
    #[serde(default)]
    to: Option<String>,
    /// Remove the cursor instead (the digest falls back to the default window).
    #[serde(default)]
    clear: bool,
}

/// Like [`parse_since`], but a relative `Nd`/`Nh` is anchored at the operation's
/// authority time so a preview and its apply plan the same cursor.
fn since_relative_to(value: &str, now: chrono::DateTime<chrono::Utc>) -> Result<chrono::DateTime<chrono::Utc>, TeamError> {
    let v = value.trim();
    let (num, unit) = v.split_at(v.len().saturating_sub(1));
    if let Ok(n) = num.parse::<i64>() {
        if (0..=36500).contains(&n) {
            match unit {
                "d" => return Ok(now - chrono::Duration::days(n)),
                "h" => return Ok(now - chrono::Duration::hours(n)),
                _ => {}
            }
        }
    }
    parse_since(v)
}

fn describe_branch(b: &Option<String>) -> String {
    b.as_deref().map(|b| format!("branch '{}'", b)).unwrap_or_else(|| "a detached HEAD".to_string())
}

pub(crate) fn plan(ctx: &mut Ctx, kind: &str, action: &Value) -> Result<Plan, TeamError> {
    let mut plan = Plan::default();
    let actor = ctx.authority.actor.clone();
    let path = cursor_path(ctx.config, &actor).ok_or_else(|| {
        TeamError::unauthorized("Unknown actors cannot mark or reset a catch-up cursor; select a member (`knobyte member select <id>`) or configure Git.")
    })?;
    let rel = ctx.rel(&path);
    let now = parse_ts(&ctx.now()).ok_or_else(|| TeamError::internal("invalid authority timestamp"))?;
    let current = get_cursor(ctx.config, &actor);
    let repo = &ctx.authority.repo_state;
    let at = match kind {
        "catchup.mark" => {
            let MarkAction { at } = parse_action(action)?;
            if let Some(c) = &current {
                if c.branch != repo.branch {
                    return Err(TeamError::conflict(format!(
                        "The catch-up cursor belongs to {}; run `knobyte catch-up reset` before using {}.",
                        describe_branch(&c.branch),
                        describe_branch(&repo.branch)
                    )));
                }
            }
            let t = match at {
                Some(s) => parse_ts(s.trim()).ok_or_else(|| TeamError::usage("'at' must be an RFC 3339 timestamp"))?,
                None => now,
            };
            if (t - now).num_seconds() > crate::team::workflow::MAX_FUTURE_SKEW_SECS {
                return Err(TeamError::validation("A catch-up cursor cannot be marked in the future"));
            }
            if let Some(prev) = current.as_ref().and_then(|c| parse_ts(&c.timestamp)) {
                if t < prev {
                    return Err(TeamError::validation(
                        "Marking would move the catch-up cursor backwards; use `knobyte catch-up reset --to <time>` instead",
                    ));
                }
            }
            Some(t)
        }
        "catchup.reset" => {
            let ResetAction { to, clear } = parse_action(action)?;
            if clear {
                if to.is_some() {
                    return Err(TeamError::usage("'clear' cannot be combined with 'to'"));
                }
                None
            } else {
                let t = match to {
                    Some(s) => since_relative_to(&s, now)?,
                    None => now,
                };
                if (t - now).num_seconds() > crate::team::workflow::MAX_FUTURE_SKEW_SECS {
                    return Err(TeamError::validation("A catch-up cursor cannot be reset into the future"));
                }
                Some(t)
            }
        }
        other => return Err(TeamError::usage(format!("Unsupported catch-up action '{}'", other))),
    };
    match at {
        Some(t) => {
            let cursor = CatchUpCursor {
                schema_version: CATCH_UP_SCHEMA_VERSION,
                actor: actor.principal(),
                actor_id: actor.id(),
                timestamp: rfc3339(t),
                branch: repo.branch.clone(),
                head: repo.head_commit.clone(),
                marked_at: ctx.now(),
            };
            plan.write_json(rel, "local", &cursor, if kind == "catchup.mark" { "Mark caught up" } else { "Reset catch-up cursor" })?;
            plan.summary = format!("{} catch-up cursor for {} at {}", if kind == "catchup.mark" { "Mark" } else { "Reset" }, actor.id(), cursor.timestamp);
            plan.result = json!(cursor);
        }
        None => {
            if path.exists() {
                plan.delete(rel, "local", "Clear catch-up cursor");
            }
            plan.summary = format!("Clear catch-up cursor for {}", actor.id());
            plan.result = Value::Null;
        }
    }
    Ok(plan)
}
