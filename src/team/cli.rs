//! Command-line surface for team commands (members, activity, workstreams,
//! specs, inbox, relays, playbooks, catch-up, log, timeline). Every mutation supports:
//!
//! * the human one-shot (default): preview and apply in one step,
//! * `--preview`: print the signed preview envelope without writing,
//! * `--apply <envelope.json>`: apply exactly that preview (refused if anything changed),
//! * `--request <file.json>`: take the action from a caller-authored request file.
//!
//! `--json` prints the schema v1 envelope `{schemaVersion, command, mode, ok, data, diagnostics, problem}`.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::{json, Value};

use crate::config::KnobyteConfig;
use crate::events::{
    append_logged_event, normalize_event_kind, parse_timeline_since, query_timeline_files, render_timeline_markdown,
    validate_timeline_files, validate_timeline_limit, validate_timeline_query, TimelineFilter,
    DEFAULT_TIMELINE_LIMIT,
};
use crate::team::activity::{activity_timeline, get_activity, list_activity_page, ActivitySubject};
use crate::team::contract::{activity_contract, inbox_contract, member_contract, relay_contract, workstream_contract};
use crate::team::envelope::{Diagnostic, TeamEnvelope, TeamError};
use crate::team::inbox::{
    get_inbox_draft, get_proposal, inbox_target, list_inbox_drafts_page, list_inbox_proposals_page, ContentPatch, EntityTarget,
    InboxChange, SpecRelation,
};
use crate::team::members::{current_actor_view, detect_git_user, get_member, list_members_page, GitAlias};
use crate::team::refs::{parse_evidence, CodeRef};
use crate::team::relay::{detect_changed_files, get_relay, get_relay_draft, list_relay_drafts_page, list_relays_page};
use crate::team::specs::{get_spec, list_specs_page, SpecListFilter};
use crate::team::workflow::{self, parse_envelope, read_bounded_json, read_request_file, ActorChoice, ApplyResult, TeamCommand};
use crate::team::workstreams::{get_workstream, list_workstreams_page};

// ---------------------------------------------------------------------------
// Shared flags
// ---------------------------------------------------------------------------

#[derive(Args, Debug, Clone, Default)]
pub struct MutationFlags {
    /// Print the signed preview envelope (exact changes) without writing anything
    #[arg(long, conflicts_with = "apply")]
    pub preview: bool,
    /// Apply the complete envelope printed by `--preview --json` (refused if anything changed)
    #[arg(long, value_name = "ENVELOPE")]
    pub apply: Option<PathBuf>,
    /// Take the action from a caller-authored request JSON file (see the `contract` subcommand)
    #[arg(long, value_name = "FILE", conflicts_with = "apply")]
    pub request: Option<PathBuf>,
    /// Stable operation id (exact replay is idempotent)
    #[arg(long = "operation-id", value_name = "ID")]
    pub operation_id: Option<String>,
    /// Emit the schema v1 team envelope
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct PageFlags {
    /// Continue a bounded result page
    #[arg(long)]
    pub cursor: Option<String>,
    /// Maximum results (1-100, default 50)
    #[arg(long)]
    pub limit: Option<usize>,
    /// Emit the schema v1 team envelope
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct JsonFlag {
    /// Emit the schema v1 team envelope
    #[arg(long)]
    pub json: bool,
}

// ---------------------------------------------------------------------------
// Output plumbing
// ---------------------------------------------------------------------------

fn print_diagnostics(diags: &[Diagnostic]) {
    for d in diags {
        let tag = match d.severity.as_str() {
            "error" => "[error]".red().bold(),
            "warning" => "[warn]".yellow().bold(),
            _ => "[info]".cyan().bold(),
        };
        eprintln!("{} {}: {}", tag, d.code, d.message);
    }
}

fn emit_error(command: &str, mode: &str, json_out: bool, err: &TeamError) -> i32 {
    let env = TeamEnvelope::error(command, mode, err);
    if json_out {
        println!("{}", env.render());
    } else {
        eprintln!("{} {}: {}", "[error]".red().bold(), err.code.as_str(), err.detail);
        if err.detail.starts_with(crate::team::inbox::SELF_APPROVAL_REQUIRED) {
            eprintln!("Hint: pass --self-approve to approve your own proposal.");
        }
    }
    env.exit_code()
}

/// Run a read command.
fn run_read(
    command: &str,
    json_out: bool,
    op: impl FnOnce() -> Result<(Value, Vec<Diagnostic>), TeamError>,
    human: impl FnOnce(&Value),
) -> i32 {
    match op() {
        Ok((data, diags)) => {
            let env = TeamEnvelope::ok(command, "read", data, diags);
            if json_out {
                println!("{}", env.render());
            } else {
                human(&env.data);
                print_diagnostics(&env.diagnostics);
            }
            env.exit_code()
        }
        Err(e) => emit_error(command, "read", json_out, &e),
    }
}

/// Run a mutation through preview / apply / one-shot.
fn run_mutation(
    config: &KnobyteConfig,
    command: &str,
    kind: &str,
    flags: &MutationFlags,
    actor: ActorChoice,
    build: impl FnOnce() -> Result<Value, TeamError>,
    human: impl FnOnce(&ApplyResult),
) -> i32 {
    if let Some(path) = &flags.apply {
        let result = std::fs::read_to_string(path)
            .map_err(|e| TeamError::usage(format!("Cannot read {}: {}", path.display(), e)))
            .and_then(|c| parse_envelope(&c))
            .and_then(|env| {
                if env.request.kind() != kind {
                    return Err(TeamError::usage(format!(
                        "The envelope previews '{}' but this command applies '{}'",
                        env.request.kind(),
                        kind
                    )));
                }
                workflow::apply(config, &env, &actor)
            });
        return finish_apply(command, flags.json, result, human);
    }

    let cmd = (|| -> Result<TeamCommand, TeamError> {
        let mut cmd = match &flags.request {
            Some(path) => read_request_file(path, Some(kind))?,
            None => TeamCommand::new(build()?),
        };
        if let Some(op) = &flags.operation_id {
            cmd.operation_id = op.clone();
        }
        Ok(cmd)
    })();
    let cmd = match cmd {
        Ok(c) => c,
        Err(e) => return emit_error(command, if flags.preview { "preview" } else { "apply" }, flags.json, &e),
    };

    if flags.preview {
        return match workflow::preview(config, &cmd, &actor) {
            Ok(env) => {
                let diags = env.preview.diagnostics.clone();
                let out = TeamEnvelope::ok(command, "preview", serde_json::to_value(&env).unwrap_or(Value::Null), diags);
                if flags.json {
                    println!("{}", out.render());
                } else {
                    println!("{} {}", "Preview:".bold(), env.preview.summary);
                    println!("Scope: {} | Operation: {}", env.preview.scope, env.request.operation_id);
                    for c in &env.preview.changes {
                        println!("  {} {} ({}) - {}", c.kind.cyan(), c.path, c.namespace, c.summary);
                    }
                    println!("Preview revision: {}", env.receipt.preview_revision);
                    print_diagnostics(&env.preview.diagnostics);
                    println!("Save it with --preview --json > preview.json, then apply with --apply preview.json (valid 30 minutes).");
                }
                out.exit_code()
            }
            Err(e) => emit_error(command, "preview", flags.json, &e),
        };
    }

    finish_apply(command, flags.json, workflow::execute(config, &cmd, &actor), human)
}

fn finish_apply(command: &str, json_out: bool, result: Result<ApplyResult, TeamError>, human: impl FnOnce(&ApplyResult)) -> i32 {
    match result {
        Ok(r) => {
            let diags = r.diagnostics.clone();
            let env = TeamEnvelope::ok(command, "apply", serde_json::to_value(&r).unwrap_or(Value::Null), diags);
            if json_out {
                println!("{}", env.render());
            } else {
                for rec in &r.recovered {
                    eprintln!("{} Recovered interrupted operation {}: {}", "[info]".cyan().bold(), rec.operation_id, rec.detail);
                }
                human(&r);
                if r.idempotent_replay {
                    println!("(replayed: operation {} was already applied)", r.operation_id);
                }
                print_diagnostics(&r.diagnostics);
            }
            env.exit_code()
        }
        Err(e) => emit_error(command, "apply", json_out, &e),
    }
}

fn ok_line(r: &ApplyResult) {
    let id = r.result.get("id").and_then(|v| v.as_str()).map(|s| format!(" ({})", s)).unwrap_or_default();
    println!("{} {}{}", "[ok]".green().bold(), r.summary, id);
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn page_continuation(data: &Value) {
    if let Some(c) = data.get("nextCursor").and_then(|c| c.as_str()) {
        println!("Next cursor: {}", c);
    }
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn required(v: Option<String>, what: &str) -> Result<String, TeamError> {
    v.ok_or_else(|| TeamError::usage(format!("{} is required (or pass --request/--apply)", what)))
}

fn parse_alias(spec: &str) -> Result<GitAlias, TeamError> {
    // "Name <email>", "<email>", "email" or "Name"
    let spec = spec.trim();
    if let (Some(lt), Some(gt)) = (spec.find('<'), spec.rfind('>')) {
        let name = spec[..lt].trim();
        let email = spec[lt + 1..gt].trim();
        return Ok(GitAlias {
            name: Some(name.to_string()).filter(|n| !n.is_empty()),
            email: Some(email.to_string()).filter(|e| !e.is_empty()),
        });
    }
    if spec.contains('@') {
        Ok(GitAlias { name: None, email: Some(spec.to_string()) })
    } else if !spec.is_empty() {
        Ok(GitAlias { name: Some(spec.to_string()), email: None })
    } else {
        Err(TeamError::usage("--alias must not be empty"))
    }
}

// ---------------------------------------------------------------------------
// member
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
pub enum MemberCommands {
    /// Versioned JSON Schema catalog of member request files
    Contract {
        /// Only this action (e.g. member.add)
        #[arg(long)]
        action: Option<String>,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// List canonical team members
    List {
        /// Only active members
        #[arg(long, conflicts_with = "inactive")]
        active: bool,
        /// Only inactive members
        #[arg(long)]
        inactive: bool,
        #[command(flatten)]
        page: PageFlags,
    },
    /// Show one member
    Show {
        id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Show the effective actor, how it was resolved, and the local selection
    Current {
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Register a new team member (name/email default to git config)
    Add {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        role: Option<String>,
        /// Git alias "Name <email>" (repeatable; defaults to name+email)
        #[arg(long = "alias")]
        aliases: Vec<String>,
        /// Also select the new member as the current member of this checkout
        #[arg(long)]
        select: bool,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Update a member's display name, email, role or Git aliases
    Update {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// New email ("" clears it)
        #[arg(long)]
        email: Option<String>,
        /// New role ("" clears it)
        #[arg(long)]
        role: Option<String>,
        /// Replace Git aliases with these ("Name <email>", repeatable)
        #[arg(long = "alias")]
        aliases: Vec<String>,
        /// Remove all Git aliases
        #[arg(long = "clear-aliases", conflicts_with = "aliases")]
        clear_aliases: bool,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Mark a member inactive (history is kept)
    Deactivate {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Restore an inactive member
    Reactivate {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Select this checkout's current member
    Select {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Clear this checkout's member selection
    Clear {
        #[command(flatten)]
        flags: MutationFlags,
    },
}

fn print_member(m: &Value) {
    println!(
        "- {} ({}) [{}]{}",
        s(m, "displayName").bold(),
        s(m, "id"),
        s(m, "status"),
        m.get("role").and_then(|r| r.as_str()).map(|r| format!(" {}", r)).unwrap_or_default()
    );
}

fn print_contract(d: &Value) {
    println!("{}", serde_json::to_string_pretty(d).unwrap_or_default());
}

pub fn run_member(config: &KnobyteConfig, cmd: MemberCommands) -> i32 {
    match cmd {
        MemberCommands::Contract { action, out } => {
            run_read("member.contract", out.json, || Ok((member_contract(action.as_deref())?, Vec::new())), print_contract)
        }
        MemberCommands::List { active, inactive, page } => {
            let filter = if active { Some(true) } else if inactive { Some(false) } else { None };
            run_read("member.list", page.json, || {
                let p = list_members_page(config, filter, page.cursor.as_deref(), page.limit)?;
                Ok((to_value(&p), Vec::new()))
            }, |d| {
                let items = d["items"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("No members found.");
                }
                items.iter().for_each(print_member);
                page_continuation(d);
            })
        }
        MemberCommands::Show { id, out } => run_read("member.show", out.json, || {
            let m = get_member(config, &id).ok_or_else(|| TeamError::not_found("Member", &id))?;
            Ok((to_value(&m), Vec::new()))
        }, |m| {
            println!("{} ({})", s(m, "displayName").bold(), s(m, "id"));
            println!("Status: {}", s(m, "status"));
            if let Some(e) = m.get("email").and_then(|e| e.as_str()) {
                println!("Email: {}", e);
            }
            if let Some(r) = m.get("role").and_then(|e| e.as_str()) {
                println!("Role: {}", r);
            }
            for a in m["gitAliases"].as_array().cloned().unwrap_or_default() {
                println!("Git alias: {} <{}>", s(&a, "name"), s(&a, "email"));
            }
        }),
        MemberCommands::Current { out } => run_read("member.current", out.json, || Ok(current_actor_view(config)), |d| {
            let source = s(d, "source");
            match d.get("member").filter(|m| !m.is_null()) {
                Some(m) => println!("Current member: {} ({}) via {}", s(m, "displayName").bold(), s(m, "id"), source),
                None => {
                    let actor = &d["actor"];
                    match s(actor, "kind").as_str() {
                        "git" => println!(
                            "No member matches; acting as Git identity {} <{}> ({}). Run `knobyte member add <id> --select`.",
                            s(actor, "name"),
                            s(actor, "email"),
                            source
                        ),
                        _ => println!("No current member and no Git identity. Run `knobyte member add <id> --select`."),
                    }
                }
            }
            if let Some(sel) = d.get("selection").filter(|v| !v.is_null()) {
                println!("Local selection: {} (since {})", s(sel, "memberId"), s(sel, "selectedAt"));
            }
        }),
        MemberCommands::Add { id, name, email, role, aliases, select, flags } => {
            if select && (flags.preview || flags.apply.is_some()) {
                return emit_error("member.add", "preview", flags.json, &TeamError::usage("--select cannot be combined with --preview/--apply; run `member select` separately"));
            }
            let id_for_select = id.clone();
            let code = run_mutation(config, "member.add", "member.add", &flags, ActorChoice::resolved(), || {
                let id = required(id, "<id>")?;
                let (git_name, git_email) = detect_git_user(&config.project_root);
                let name = name.or(git_name).ok_or_else(|| TeamError::usage("No --name given and git user.name is not configured"))?;
                let email = email.or(git_email);
                let mut member = json!({ "id": id, "displayName": name, "email": email, "role": role });
                if !aliases.is_empty() {
                    let parsed = aliases.iter().map(|a| parse_alias(a)).collect::<Result<Vec<_>, _>>()?;
                    member["gitAliases"] = to_value(&parsed);
                }
                Ok(json!({ "kind": "member.add", "member": member }))
            }, ok_line);
            if code == 0 && select {
                if let Some(id) = id_for_select {
                    return run_mutation(config, "member.select", "member.select", &MutationFlags { json: flags.json, ..Default::default() }, ActorChoice::resolved(), || Ok(json!({ "kind": "member.select", "memberId": id })), ok_line);
                }
            }
            code
        }
        MemberCommands::Update { id, name, email, role, aliases, clear_aliases, flags } => {
            run_mutation(config, "member.update", "member.update", &flags, ActorChoice::resolved(), || {
                let id = required(id, "<id>")?;
                let mut patch = serde_json::Map::new();
                if let Some(n) = name {
                    patch.insert("displayName".into(), json!(n));
                }
                if let Some(e) = email {
                    patch.insert("email".into(), json!(e));
                }
                if let Some(r) = role {
                    patch.insert("role".into(), json!(r));
                }
                if clear_aliases {
                    patch.insert("gitAliases".into(), json!([]));
                } else if !aliases.is_empty() {
                    let parsed = aliases.iter().map(|a| parse_alias(a)).collect::<Result<Vec<_>, _>>()?;
                    patch.insert("gitAliases".into(), to_value(&parsed));
                }
                if patch.is_empty() {
                    return Err(TeamError::usage("Nothing to update: pass --name, --email, --role, --alias or --clear-aliases"));
                }
                Ok(json!({ "kind": "member.update", "memberId": id, "patch": patch }))
            }, ok_line)
        }
        MemberCommands::Deactivate { id, flags } => run_mutation(config, "member.deactivate", "member.deactivate", &flags, ActorChoice::resolved(), || Ok(json!({ "kind": "member.deactivate", "memberId": required(id, "<id>")? })), ok_line),
        MemberCommands::Reactivate { id, flags } => run_mutation(config, "member.reactivate", "member.reactivate", &flags, ActorChoice::resolved(), || Ok(json!({ "kind": "member.reactivate", "memberId": required(id, "<id>")? })), ok_line),
        MemberCommands::Select { id, flags } => run_mutation(config, "member.select", "member.select", &flags, ActorChoice::resolved(), || Ok(json!({ "kind": "member.select", "memberId": required(id, "<id>")? })), ok_line),
        MemberCommands::Clear { flags } => run_mutation(config, "member.clear", "member.clear", &flags, ActorChoice::resolved(), || Ok(json!({ "kind": "member.clear" })), ok_line),
    }
}

// ---------------------------------------------------------------------------
// activity
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
pub enum ActivityCommands {
    /// Versioned JSON Schema catalog of activity request files
    Contract {
        /// Only this action (e.g. activity.record)
        #[arg(long)]
        action: Option<String>,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// List canonical activity, newest first
    List {
        /// Only records at or after: RFC 3339, YYYY-MM-DD, or relative Nd/Nh
        #[arg(long)]
        since: Option<String>,
        #[command(flatten)]
        page: PageFlags,
    },
    /// Show one activity record
    Show {
        id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Record a custom activity event
    Record {
        #[arg(required_unless_present_any = ["apply", "request"])]
        action: Option<String>,
        #[arg(required_unless_present_any = ["apply", "request"])]
        summary: Option<String>,
        /// Entity kind of the subject (default: general)
        #[arg(long)]
        kind: Option<String>,
        /// Entity id of the subject
        #[arg(long)]
        target: Option<String>,
        /// Typed subject: entity:<kind>:<id>, code:<symbol>, file:<path>, commit:<hash> (repeatable)
        #[arg(long = "subject")]
        subjects: Vec<String>,
        /// Related workstream id
        #[arg(long)]
        workstream: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Merged timeline of canonical activity and the decision/event log
    Timeline {
        /// Only one source: activity or log
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[command(flatten)]
        page: PageFlags,
    },
}

fn print_activity(a: &Value) {
    println!("[{}] {}: {} ({})", s(a, "timestamp").dimmed(), s(a, "actor").bold(), s(a, "summary"), s(a, "action"));
}

pub fn run_activity(config: &KnobyteConfig, cmd: ActivityCommands) -> i32 {
    match cmd {
        ActivityCommands::Contract { action, out } => {
            run_read("activity.contract", out.json, || Ok((activity_contract(action.as_deref())?, Vec::new())), print_contract)
        }
        ActivityCommands::List { since, page } => run_read("activity.list", page.json, || {
            Ok((to_value(&list_activity_page(config, since.as_deref(), page.cursor.as_deref(), page.limit)?), Vec::new()))
        }, |d| {
            let items = d["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("No activity found.");
            }
            items.iter().for_each(print_activity);
            page_continuation(d);
        }),
        ActivityCommands::Show { id, out } => run_read("activity.show", out.json, || {
            let a = get_activity(config, &id).ok_or_else(|| TeamError::not_found("Activity event", &id))?;
            Ok((to_value(&a), Vec::new()))
        }, |a| {
            print_activity(a);
            println!("Entity: {} {} ({})", s(a, "entityKind"), s(a, "entityId"), s(a, "entityTitle"));
            if let Some(o) = a.get("origin") {
                println!("Origin: {}", o);
            }
            if let Some(r) = a.get("repoState") {
                println!(
                    "Repository: {} @ {}{}",
                    s(r, "branch"),
                    s(r, "headCommit"),
                    if r["dirtyTree"].as_bool().unwrap_or(false) { " (dirty)" } else { "" }
                );
            }
            for sub in a["subjects"].as_array().cloned().unwrap_or_default() {
                println!("Subject: {}", sub);
            }
        }),
        ActivityCommands::Record { action, summary, kind, target, subjects, workstream, flags } => {
            run_mutation(config, "activity.record", "activity.record", &flags, ActorChoice::resolved(), || {
                let mut subs = subjects.iter().map(|x| ActivitySubject::parse(x)).collect::<Result<Vec<_>, _>>().map_err(TeamError::usage)?;
                if let (Some(k), Some(t)) = (&kind, &target) {
                    subs.insert(0, ActivitySubject::entity(k, t, None));
                }
                Ok(json!({ "kind": "activity.record", "activity": {
                    "action": required(action, "<action>")?,
                    "summary": required(summary, "<summary>")?,
                    "entityKind": kind,
                    "entityId": target,
                    "subjects": subs,
                    "workstream": workstream,
                }}))
            }, |r| println!("{} Recorded activity: {}", "[ok]".green().bold(), s(&r.result, "summary")))
        }
        ActivityCommands::Timeline { source, since, page } => run_read("activity.timeline", page.json, || {
            Ok((to_value(&activity_timeline(config, source.as_deref(), since.as_deref(), page.cursor.as_deref(), page.limit)?), Vec::new()))
        }, |d| {
            let items = d["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("No timeline entries found.");
            }
            for i in items {
                println!("[{}] {} {} {} - {}", s(&i, "timestamp").dimmed(), s(&i, "source"), s(&i, "kind").cyan(), s(&i, "actor").bold(), s(&i, "summary"));
            }
            page_continuation(d);
        }),
    }
}

// ---------------------------------------------------------------------------
// workstream
// ---------------------------------------------------------------------------

#[derive(Args, Debug, Clone, Default)]
pub struct WorkstreamFields {
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub goal: Option<String>,
    #[arg(long)]
    pub summary: Option<String>,
    /// planned, active, blocked or done
    #[arg(long)]
    pub state: Option<String>,
    /// Owner member id (repeatable)
    #[arg(long = "owner")]
    pub owners: Vec<String>,
    #[arg(long = "contributor")]
    pub contributors: Vec<String>,
    /// Repository path in scope (repeatable)
    #[arg(long = "path")]
    pub paths: Vec<String>,
    /// Code symbol in scope (repeatable)
    #[arg(long = "code")]
    pub code: Vec<String>,
    #[arg(long = "topic")]
    pub topics: Vec<String>,
    #[arg(long = "component")]
    pub components: Vec<String>,
    #[arg(long = "related")]
    pub related: Vec<String>,
    #[arg(long = "next-milestone")]
    pub next_milestone: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum WorkstreamCommands {
    /// Versioned JSON Schema catalog of workstream request files
    Contract {
        /// Only this action (e.g. workstream.create)
        #[arg(long)]
        action: Option<String>,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// List workstreams (archived hidden unless requested)
    List {
        /// Filter by lifecycle state (repeatable)
        #[arg(long = "state")]
        states: Vec<String>,
        #[arg(long = "include-archived")]
        include_archived: bool,
        #[command(flatten)]
        page: PageFlags,
    },
    /// Show one workstream
    Show {
        id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Create a workstream
    Create {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[arg(required_unless_present_any = ["apply", "request"])]
        title: Option<String>,
        #[command(flatten)]
        fields: WorkstreamFields,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Update a workstream
    Update {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[command(flatten)]
        fields: WorkstreamFields,
        /// Replace blockers (repeatable)
        #[arg(long = "blocker")]
        blockers: Vec<String>,
        #[arg(long = "clear-blockers", conflicts_with = "blockers")]
        clear_blockers: bool,
        #[arg(long = "current-state")]
        current_state: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Archive a workstream
    Archive {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
}

fn opt_list(v: Vec<String>) -> Option<Vec<String>> {
    if v.is_empty() { None } else { Some(v) }
}

pub fn run_workstream(config: &KnobyteConfig, cmd: WorkstreamCommands) -> i32 {
    match cmd {
        WorkstreamCommands::Contract { action, out } => {
            run_read("workstream.contract", out.json, || Ok((workstream_contract(action.as_deref())?, Vec::new())), print_contract)
        }
        WorkstreamCommands::List { states, include_archived, page } => run_read("workstream.list", page.json, || {
            Ok((to_value(&list_workstreams_page(config, &states, include_archived, page.cursor.as_deref(), page.limit)?), Vec::new()))
        }, |d| {
            let items = d["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("No workstreams found.");
            }
            for w in items {
                println!("- {} ({}) [{}]", s(&w, "title").bold(), s(&w, "id"), s(&w, "status"));
            }
            page_continuation(d);
        }),
        WorkstreamCommands::Show { id, out } => run_read("workstream.show", out.json, || {
            let w = get_workstream(config, &id).ok_or_else(|| TeamError::not_found("Workstream", &id))?;
            Ok((to_value(&w), Vec::new()))
        }, |w| {
            println!("# {} ({}) [{}]", s(w, "title").bold(), s(w, "id"), s(w, "status"));
            for (label, key) in [("Goal", "goal"), ("Summary", "summary"), ("Current state", "currentState"), ("Next milestone", "nextMilestone"), ("Description", "description")] {
                let v = s(w, key);
                if !v.is_empty() {
                    println!("{}: {}", label, v);
                }
            }
            for (label, key) in [("Owners", "owners"), ("Contributors", "contributors"), ("Paths", "paths"), ("Code", "code"), ("Topics", "topics"), ("Components", "components"), ("Related", "related"), ("Blockers", "blockers")] {
                let items: Vec<String> = w[key].as_array().cloned().unwrap_or_default().iter().filter_map(|x| x.as_str().map(String::from)).collect();
                if !items.is_empty() {
                    println!("{}: {}", label, items.join(", "));
                }
            }
            for st in w["steps"].as_array().cloned().unwrap_or_default() {
                println!("  step {} [{}] {}", s(&st, "id"), s(&st, "status"), s(&st, "title"));
            }
        }),
        WorkstreamCommands::Create { id, title, fields, flags } => run_mutation(config, "workstream.create", "workstream.create", &flags, ActorChoice::resolved(), || {
            Ok(json!({ "kind": "workstream.create", "workstream": {
                "id": required(id, "<id>")?,
                "title": required(title, "<title>")?,
                "description": fields.description,
                "goal": fields.goal,
                "summary": fields.summary,
                "state": fields.state,
                "owners": fields.owners,
                "contributors": fields.contributors,
                "paths": fields.paths,
                "code": fields.code,
                "topics": fields.topics,
                "components": fields.components,
                "related": fields.related,
                "nextMilestone": fields.next_milestone,
            }}))
        }, ok_line),
        WorkstreamCommands::Update { id, title, fields, blockers, clear_blockers, current_state, flags } => {
            run_mutation(config, "workstream.update", "workstream.update", &flags, ActorChoice::resolved(), || {
                let patch = crate::team::workstreams::WorkstreamPatch {
                    title,
                    description: fields.description,
                    goal: fields.goal,
                    summary: fields.summary,
                    state: fields.state,
                    owners: opt_list(fields.owners),
                    contributors: opt_list(fields.contributors),
                    paths: opt_list(fields.paths),
                    code: opt_list(fields.code),
                    topics: opt_list(fields.topics),
                    components: opt_list(fields.components),
                    related: opt_list(fields.related),
                    blockers: if clear_blockers { Some(Vec::new()) } else { opt_list(blockers) },
                    current_state,
                    next_milestone: fields.next_milestone,
                };
                Ok(json!({ "kind": "workstream.update", "workstreamId": required(id, "<id>")?, "patch": patch }))
            }, ok_line)
        }
        WorkstreamCommands::Archive { id, flags } => run_mutation(config, "workstream.archive", "workstream.archive", &flags, ActorChoice::resolved(), || {
            Ok(json!({ "kind": "workstream.archive", "workstreamId": required(id, "<id>")? }))
        }, ok_line),
    }
}

// ---------------------------------------------------------------------------
// spec
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
pub enum SpecCommands {
    /// List specs with lifecycle and grounding health
    List {
        /// in_flight, promoted, deprecated or archived
        #[arg(long)]
        lifecycle: Option<String>,
        /// fresh, changed, missing, ambiguous or unverified
        #[arg(long)]
        grounding: Option<String>,
        #[arg(long)]
        topic: Option<String>,
        #[arg(long = "include-archived")]
        include_archived: bool,
        #[command(flatten)]
        page: PageFlags,
    },
    /// Show a spec (id or path such as specs/auth.md) with its requirements,
    /// acceptance criteria, constraints and grounding rollup
    Show {
        id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
}

pub fn run_spec(config: &KnobyteConfig, cmd: SpecCommands) -> i32 {
    match cmd {
        SpecCommands::List { lifecycle, grounding, topic, include_archived, page } => run_read("spec.list", page.json, || {
            let f = SpecListFilter { lifecycle, grounding, topic, include_archived, cursor: page.cursor.clone(), limit: page.limit };
            Ok((to_value(&list_specs_page(config, &f)?), Vec::new()))
        }, |d| {
            let items = d["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("No specs found.");
            }
            for sp in items {
                println!("- {} ({}) [{} / {}]", s(&sp, "title").bold(), s(&sp, "id"), s(&sp, "lifecycleState"), s(&sp, "groundingHealth"));
            }
            page_continuation(d);
        }),
        SpecCommands::Show { id, out } => run_read("spec.show", out.json, || {
            let sp = get_spec(config, &id).map_err(TeamError::from)?;
            Ok((to_value(&sp), Vec::new()))
        }, |sp| {
            println!("# {} ({})", s(sp, "title").bold(), s(sp, "id"));
            println!("Lifecycle: {} | Grounding: {} | File: {}", s(sp, "lifecycleState"), s(sp, "groundingHealth"), s(sp, "file"));
            if let Some(sm) = sp.get("summary").and_then(|x| x.as_str()) {
                println!("Summary: {}", sm);
            }
            for (label, key) in [("Requirements", "requirements"), ("Acceptance criteria", "acceptanceCriteria"), ("Constraints", "constraints")] {
                let items = sp["hierarchy"][key].as_array().cloned().unwrap_or_default();
                if !items.is_empty() {
                    println!("{}:", label.bold());
                    for i in items {
                        println!("  - {} ({}) [{}]", s(&i, "title"), s(&i, "id"), s(&i, "groundingHealth"));
                    }
                }
            }
            if let Some(r) = sp.get("groundingRollup").and_then(|r| r.as_object()).filter(|r| !r.is_empty()) {
                println!("Groundings: {}", r.iter().map(|(k, v)| format!("{} {}", v, k)).collect::<Vec<_>>().join(", "));
            }
            println!("---\n{}", s(sp, "body"));
        }),
    }
}

// ---------------------------------------------------------------------------
// inbox
// ---------------------------------------------------------------------------

#[derive(Args, Debug, Clone, Default)]
pub struct InboxDraftFields {
    #[arg(long)]
    pub title: Option<String>,
    /// Legacy Markdown edit: target document under .knobyte/ (e.g. context/rate-limit.md)
    #[arg(long)]
    pub target: Option<String>,
    /// Legacy Markdown edit: content to append or replace
    #[arg(long)]
    pub content: Option<String>,
    /// Legacy Markdown edit: append (default) or replace
    #[arg(long)]
    pub mode: Option<String>,
    /// Typed change: knowledge.create, knowledge.update, spec.create or spec.update
    #[arg(long)]
    pub change: Option<String>,
    /// Entity kind for *.create (architecture, component, convention, decision, pattern, guide; spec, requirement, constraint, acceptance_criterion)
    #[arg(long = "kind")]
    pub entity_kind: Option<String>,
    /// Target entity id for *.update (see `knobyte inbox target <id>`)
    #[arg(long)]
    pub entity: Option<String>,
    /// Markdown body for a create, or the replacement body for an update
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long)]
    pub summary: Option<String>,
    /// in_flight (default) or promoted, for *.create
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long = "topic")]
    pub topics: Vec<String>,
    /// spec.create relation <type>:<target-id> (derived_from, refines, constrained_by, verified_by)
    #[arg(long)]
    pub relation: Option<String>,
    /// Why this change matters
    #[arg(long)]
    pub reason: Option<String>,
    /// Evidence: entity:<id>, code:<symbol>, commit:<hash>, file:<path>, URL, or a note (repeatable)
    #[arg(long)]
    pub evidence: Vec<String>,
    /// Pin the target revision read with `inbox target` (sha256:...)
    #[arg(long = "target-revision")]
    pub target_revision: Option<String>,
}

impl InboxDraftFields {
    fn to_input(&self, config: &KnobyteConfig) -> Result<Value, TeamError> {
        let reason = self.reason.clone().ok_or_else(|| TeamError::usage("--reason is required"))?;
        let evidence = parse_evidence(&self.evidence).map_err(TeamError::usage)?;
        let mut input = json!({ "rationale": reason, "evidence": evidence });
        if let Some(t) = &self.title {
            input["title"] = json!(t);
        }
        match self.change.as_deref() {
            None => {
                input["target"] = json!(self.target.clone().ok_or_else(|| TeamError::usage("--target is required (or use --change)"))?);
                input["content"] = json!(self.content.clone().ok_or_else(|| TeamError::usage("--content is required (or use --change)"))?);
                if let Some(m) = &self.mode {
                    input["mode"] = json!(m);
                }
                if let Some(rev) = &self.target_revision {
                    let target = crate::team::inbox::normalize_proposal_target(self.target.as_deref().unwrap_or("")).map_err(TeamError::usage)?;
                    input["targetRevisions"] = json!([{ "path": target, "revision": rev }]);
                }
            }
            Some(kind) => {
                if self.target.is_some() || self.content.is_some() {
                    return Err(TeamError::usage("--change cannot be combined with --target/--content"));
                }
                let change = match kind {
                    "knowledge.create" | "spec.create" => {
                        let entity_kind = self.entity_kind.clone().ok_or_else(|| TeamError::usage("--kind is required for a create"))?;
                        let title = self.title.clone().ok_or_else(|| TeamError::usage("--title is required for a create"))?;
                        let body = self.body.clone().ok_or_else(|| TeamError::usage("--body is required for a create"))?;
                        if kind == "knowledge.create" {
                            if self.relation.is_some() {
                                return Err(TeamError::usage("--relation is only valid with --change spec.create"));
                            }
                            InboxChange::KnowledgeCreate { entity_kind, title, body, summary: self.summary.clone(), status: self.status.clone(), topics: self.topics.clone() }
                        } else {
                            let relation = match &self.relation {
                                None => None,
                                Some(r) => {
                                    let (t, id) = r.split_once(':').ok_or_else(|| TeamError::usage("--relation must be <type>:<target-id>"))?;
                                    Some(SpecRelation { rel_type: t.to_string(), target: EntityTarget { id: id.to_string(), kind: None, title: None } })
                                }
                            };
                            InboxChange::SpecCreate { entity_kind, title, body, summary: self.summary.clone(), status: self.status.clone(), topics: self.topics.clone(), relation }
                        }
                    }
                    "knowledge.update" | "spec.update" => {
                        let id = self.entity.clone().ok_or_else(|| TeamError::usage("--entity <id> is required for an update"))?;
                        let target = EntityTarget { id: id.clone(), kind: None, title: None };
                        let patch = ContentPatch { title: self.title.clone(), summary: self.summary.clone(), body: self.body.clone() };
                        if let Some(rev) = &self.target_revision {
                            let found = crate::team::inbox::find_entity(config, &id).ok_or_else(|| TeamError::not_found("Knowledge target", &id))?;
                            input["targetRevisions"] = json!([{ "path": found.rel, "revision": rev }]);
                        }
                        if kind == "knowledge.update" { InboxChange::KnowledgeUpdate { target, patch } } else { InboxChange::SpecUpdate { target, patch } }
                    }
                    other => return Err(TeamError::usage(format!("Unknown --change '{}'", other))),
                };
                input["change"] = to_value(&change);
            }
        }
        Ok(input)
    }
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum InboxDraftCommands {
    /// List local drafts
    List {
        #[command(flatten)]
        page: PageFlags,
    },
    /// Show one complete local draft
    Show {
        id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Save (create or, with --draft-id, replace) a local draft
    Save {
        /// Replace this existing draft
        #[arg(long = "draft-id")]
        draft_id: Option<String>,
        #[command(flatten)]
        fields: InboxDraftFields,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Delete a local draft
    Delete {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
}

#[derive(Args, Debug, Clone, Default)]
pub struct ReviewArgs {
    #[arg(required_unless_present_any = ["apply", "request"])]
    pub id: Option<String>,
    /// Review rationale
    #[arg(long, alias = "reason")]
    pub note: Option<String>,
    /// Expected reviewer: must be the current member (refused otherwise)
    #[arg(long)]
    pub member: Option<String>,
    #[command(flatten)]
    pub flags: MutationFlags,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum InboxProposalCommands {
    /// List proposals
    List {
        /// Filter by state: pending, approved, rejected, withdrawn, stale (repeatable)
        #[arg(long = "state", alias = "status")]
        states: Vec<String>,
        #[command(flatten)]
        page: PageFlags,
    },
    /// Show one proposal
    Show {
        id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Approve a pending proposal and apply its knowledge change
    Approve {
        #[command(flatten)]
        review: ReviewArgs,
        /// Approve your own proposal without teammate review
        #[arg(long = "self-approve")]
        self_approve: bool,
    },
    /// Reject a pending proposal
    Reject {
        #[command(flatten)]
        review: ReviewArgs,
    },
    /// Withdraw your own pending proposal
    Withdraw {
        #[command(flatten)]
        review: ReviewArgs,
    },
    /// Mark a pending proposal stale after its target changed
    MarkStale {
        #[command(flatten)]
        review: ReviewArgs,
    },
    /// Repair a stale proposal back to pending with new content
    Repair {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        /// Use this local draft's content as the replacement
        #[arg(long = "from-draft", value_name = "DRAFT_ID")]
        from_draft: Option<String>,
        #[command(flatten)]
        fields: InboxDraftFields,
        #[command(flatten)]
        flags: MutationFlags,
    },
}

#[derive(Subcommand, Debug)]
pub enum InboxCommands {
    /// Resolve an existing knowledge record and its exact revision for a correction
    Target {
        id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Versioned JSON Schema catalog of Inbox request files
    Contract {
        /// Only this action (e.g. inbox.draft.save)
        #[arg(long)]
        action: Option<String>,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Local inbox drafts (list, show, save, delete)
    Draft {
        #[command(subcommand)]
        sub: InboxDraftCommands,
    },
    /// Publish a local draft as a pending proposal
    Publish {
        #[arg(required_unless_present_any = ["apply", "request"])]
        draft_id: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Published proposals (list, show, approve, reject, withdraw, mark-stale, repair)
    Proposal {
        #[command(subcommand)]
        sub: InboxProposalCommands,
    },
    /// Approve a pending proposal (shorthand for `inbox proposal approve`)
    Approve {
        #[command(flatten)]
        review: ReviewArgs,
        /// Approve your own proposal without teammate review
        #[arg(long = "self-approve")]
        self_approve: bool,
    },
    /// Reject a pending proposal (shorthand for `inbox proposal reject`)
    Reject {
        #[command(flatten)]
        review: ReviewArgs,
    },
    /// Withdraw your own pending proposal (shorthand)
    Withdraw {
        #[command(flatten)]
        review: ReviewArgs,
    },
}

fn print_proposal(p: &Value) {
    println!("# {} ({})", s(p, "title").bold(), s(p, "id"));
    println!("State: {} | Target: .knobyte/{} | Change: {}", s(p, "status").cyan(), s(p, "target"), p.get("change").and_then(|c| c.get("kind")).and_then(|k| k.as_str()).map(String::from).unwrap_or_else(|| format!("content.{}", s(p, "mode"))));
    println!("Author: {} | Created: {}", s(p, "author"), s(p, "createdAt"));
    println!("Rationale: {}", s(p, "reason"));
    for e in p["evidence"].as_array().cloned().unwrap_or_default() {
        println!("Evidence: {}", e);
    }
    if let Some(by) = p.get("decisionBy").and_then(|x| x.as_str()) {
        println!(
            "Decision by {}{}{}{}",
            by,
            p.get("decidedAt").and_then(|x| x.as_str()).map(|d| format!(" at {}", d)).unwrap_or_default(),
            p.get("decisionReason").and_then(|x| x.as_str()).map(|r| format!(": {}", r)).unwrap_or_default(),
            if p["selfApproved"].as_bool().unwrap_or(false) { " (self-approved)" } else { "" }
        );
    }
    if let Some(r) = p.get("staleReason").and_then(|x| x.as_str()) {
        println!("Stale: {}", r);
    }
    println!("---\n{}", s(p, "proposedContent"));
}

fn review(config: &KnobyteConfig, command: &str, kind: &str, args: ReviewArgs, self_approve: bool) -> i32 {
    let actor = match &args.member {
        Some(m) => ActorChoice::member(m),
        None => ActorChoice::resolved(),
    };
    run_mutation(config, command, kind, &args.flags, actor, || {
        let mut a = json!({ "kind": kind, "proposalId": required(args.id.clone(), "<proposal-id>")? });
        if let Some(n) = &args.note {
            a["rationale"] = json!(n);
        }
        if kind == "inbox.approve" && self_approve {
            a["selfApprove"] = json!(true);
        }
        Ok(a)
    }, |r| {
        let p = &r.result;
        match kind {
            "inbox.approve" => println!("{} Approved proposal '{}' -> .knobyte/{}", "[ok]".green().bold(), s(p, "id"), s(p, "target")),
            _ => println!("{} {} ({})", "[ok]".green().bold(), r.summary, s(p, "id")),
        }
    })
}

pub fn run_inbox(config: &KnobyteConfig, cmd: InboxCommands) -> i32 {
    match cmd {
        InboxCommands::Target { id, out } => run_read("inbox.target", out.json, || Ok((inbox_target(config, &id)?, Vec::new())), |d| {
            println!("{} ({})", s(&d["target"], "title").bold(), s(&d["target"], "id"));
            println!("Kind: {} | Source: {}", s(&d["target"], "kind"), s(d, "sourcePath"));
            println!("Revision: {} / {}", d["version"]["semanticRevision"], s(&d["version"], "contentHash"));
            println!("---\n{}", s(d, "body"));
        }),
        InboxCommands::Contract { action, out } => run_read("inbox.contract", out.json, || Ok((inbox_contract(action.as_deref())?, Vec::new())), |d| {
            println!("{}", serde_json::to_string_pretty(d).unwrap_or_default());
        }),
        InboxCommands::Draft { sub } => match sub {
            InboxDraftCommands::List { page } => run_read("inbox.draft.list", page.json, || {
                Ok((to_value(&list_inbox_drafts_page(config, page.cursor.as_deref(), page.limit)?), Vec::new()))
            }, |d| {
                let items = d["items"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("No local inbox drafts found.");
                }
                for x in items {
                    let ck = x.get("change").and_then(|c| c.get("kind")).and_then(|k| k.as_str()).unwrap_or("content");
                    println!("- [{}] {} ({}) {}", s(&x, "target").cyan(), s(&x, "title").bold(), s(&x, "id"), ck);
                }
                page_continuation(d);
            }),
            InboxDraftCommands::Show { id, out } => run_read("inbox.draft.show", out.json, || {
                let d = get_inbox_draft(config, &id).ok_or_else(|| TeamError::not_found("Inbox draft", &id))?;
                Ok((to_value(&d), Vec::new()))
            }, |d| {
                println!("{} ({})", s(d, "title").bold(), s(d, "id"));
                println!("Target: .knobyte/{} | Author: {}", s(d, "target"), s(d, "author"));
                println!("Rationale: {}", s(d, "reason"));
                if let Some(c) = d.get("change") {
                    println!("Change: {}", c);
                }
                println!("---\n{}", s(d, "proposedContent"));
            }),
            InboxDraftCommands::Save { draft_id, fields, flags } => run_mutation(config, "inbox.draft.save", "inbox.draft.save", &flags, ActorChoice::resolved(), || {
                let mut a = json!({ "kind": "inbox.draft.save", "draft": fields.to_input(config)? });
                if let Some(id) = draft_id {
                    a["draftId"] = json!(id);
                }
                Ok(a)
            }, |r| {
                println!(
                    "{} Saved inbox draft: {} (target: {}{})",
                    "[ok]".green().bold(),
                    s(&r.result, "id"),
                    s(&r.result, "target"),
                    r.result.get("mode").and_then(|m| m.as_str()).map(|m| format!(", mode: {}", m)).unwrap_or_default()
                )
            }),
            InboxDraftCommands::Delete { id, flags } => run_mutation(config, "inbox.draft.delete", "inbox.draft.delete", &flags, ActorChoice::resolved(), || {
                Ok(json!({ "kind": "inbox.draft.delete", "draftId": required(id, "<draft-id>")? }))
            }, ok_line),
        },
        InboxCommands::Publish { draft_id, flags } => run_mutation(config, "inbox.publish", "inbox.publish", &flags, ActorChoice::resolved(), || {
            Ok(json!({ "kind": "inbox.publish", "draftId": required(draft_id, "<draft-id>")? }))
        }, |r| println!("{} Published inbox proposal '{}': {}", "[ok]".green().bold(), s(&r.result, "id"), s(&r.result, "title"))),
        InboxCommands::Approve { review: a, self_approve } => review(config, "inbox.proposal.approve", "inbox.approve", a, self_approve),
        InboxCommands::Reject { review: a } => review(config, "inbox.proposal.reject", "inbox.reject", a, false),
        InboxCommands::Withdraw { review: a } => review(config, "inbox.proposal.withdraw", "inbox.withdraw", a, false),
        InboxCommands::Proposal { sub } => match sub {
            InboxProposalCommands::List { states, page } => run_read("inbox.proposal.list", page.json, || {
                Ok((to_value(&list_inbox_proposals_page(config, &states, page.cursor.as_deref(), page.limit)?), Vec::new()))
            }, |d| {
                let items = d["items"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("No inbox proposals found.");
                }
                for p in items {
                    println!("- [{}] {} ({}) - {}", s(&p, "status").cyan(), s(&p, "title").bold(), s(&p, "id"), s(&p, "target"));
                }
                page_continuation(d);
            }),
            InboxProposalCommands::Show { id, out } => run_read("inbox.proposal.show", out.json, || {
                let p = get_proposal(config, &id).map_err(TeamError::from)?;
                Ok((to_value(&p), Vec::new()))
            }, print_proposal),
            InboxProposalCommands::Approve { review: a, self_approve } => review(config, "inbox.proposal.approve", "inbox.approve", a, self_approve),
            InboxProposalCommands::Reject { review: a } => review(config, "inbox.proposal.reject", "inbox.reject", a, false),
            InboxProposalCommands::Withdraw { review: a } => review(config, "inbox.proposal.withdraw", "inbox.withdraw", a, false),
            InboxProposalCommands::MarkStale { review: a } => review(config, "inbox.proposal.mark-stale", "inbox.mark-stale", a, false),
            InboxProposalCommands::Repair { id, from_draft, fields, flags } => run_mutation(config, "inbox.proposal.repair", "inbox.repair", &flags, ActorChoice::resolved(), || {
                let replacement = match from_draft {
                    Some(did) => {
                        let d = get_inbox_draft(config, &did).ok_or_else(|| TeamError::not_found("Inbox draft", &did))?;
                        let mut v = json!({ "title": d.title, "rationale": d.reason, "evidence": d.evidence, "targetRevisions": d.target_revisions });
                        match d.change {
                            Some(c) => v["change"] = to_value(&c),
                            None => {
                                v["target"] = json!(d.target);
                                v["content"] = json!(d.proposed_content);
                                v["mode"] = json!(d.mode.unwrap_or_else(|| "append".to_string()));
                            }
                        }
                        v
                    }
                    None => fields.to_input(config)?,
                };
                Ok(json!({ "kind": "inbox.repair", "proposalId": required(id, "<proposal-id>")?, "replacement": replacement }))
            }, ok_line),
        },
    }
}

// ---------------------------------------------------------------------------
// relay
// ---------------------------------------------------------------------------

#[derive(Args, Debug, Clone, Default)]
pub struct RelayDraftFields {
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub summary: Option<String>,
    /// team (any active member may claim) or members (named recipients only)
    #[arg(long)]
    pub audience: Option<String>,
    /// Named recipient member id (repeatable, at most 32)
    #[arg(long = "to")]
    pub to: Vec<String>,
    /// Completed work (repeatable)
    #[arg(long)]
    pub completed: Vec<String>,
    /// Work in progress (repeatable)
    #[arg(long = "in-progress")]
    pub in_progress: Vec<String>,
    /// Legacy progress note (repeatable)
    #[arg(long)]
    pub progress: Vec<String>,
    /// Decision made (repeatable)
    #[arg(long = "decision")]
    pub decisions: Vec<String>,
    #[arg(long = "blocker")]
    pub blockers: Vec<String>,
    /// Unresolved question (repeatable)
    #[arg(long = "question")]
    pub questions: Vec<String>,
    /// Next action (repeatable)
    #[arg(long = "next")]
    pub next: Vec<String>,
    /// Changed file (repeatable; default: detected from git)
    #[arg(long = "changed-file")]
    pub changed_files: Vec<String>,
    /// Do not detect changed files from git
    #[arg(long = "no-auto-files")]
    pub no_auto_files: bool,
    /// Code reference: symbol id or file:<path> (repeatable)
    #[arg(long)]
    pub code: Vec<String>,
    /// Evidence: entity:<id>, code:<symbol>, commit:<hash>, file:<path>, URL, or a note (repeatable)
    #[arg(long)]
    pub evidence: Vec<String>,
    #[arg(long)]
    pub workstream: Option<String>,
}

impl RelayDraftFields {
    fn to_input(&self, config: &KnobyteConfig) -> Result<Value, TeamError> {
        let summary = self.summary.clone().ok_or_else(|| TeamError::usage("--summary is required"))?;
        let code = self.code.iter().map(|c| CodeRef::parse(c)).collect::<Result<Vec<_>, _>>().map_err(TeamError::usage)?;
        let evidence = parse_evidence(&self.evidence).map_err(TeamError::usage)?;
        let changed = if self.changed_files.is_empty() && !self.no_auto_files { detect_changed_files(&config.project_root) } else { self.changed_files.clone() };
        Ok(json!({
            "title": self.title,
            "summary": summary,
            "audience": self.audience,
            "recipients": self.to,
            "completed": self.completed,
            "inProgress": self.in_progress,
            "progress": self.progress,
            "decisions": self.decisions,
            "blockers": self.blockers,
            "unresolvedQuestions": self.questions,
            "nextActions": self.next,
            "changedFiles": changed,
            "code": code,
            "evidence": evidence,
            "workstream": self.workstream,
        }))
    }
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum RelayDraftCommands {
    /// List local relay drafts
    List {
        #[command(flatten)]
        page: PageFlags,
    },
    /// Show one complete local relay draft
    Show {
        id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Save (create or, with --draft-id, replace) a local relay draft
    Save {
        /// Replace this existing draft
        #[arg(long = "draft-id")]
        draft_id: Option<String>,
        /// Create the draft from sparse JSON content (fields of the relay draft contract)
        #[arg(long = "from", value_name = "DRAFT_FILE", conflicts_with = "request")]
        from: Option<PathBuf>,
        /// Expected actor: must be the current member (refused otherwise)
        #[arg(long)]
        sender: Option<String>,
        #[command(flatten)]
        fields: RelayDraftFields,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Delete a local relay draft
    Delete {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum RelayCommands {
    /// Versioned JSON Schema catalog of Relay request files
    Contract {
        #[arg(long)]
        action: Option<String>,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Local relay drafts (list, show, save, delete)
    Draft {
        #[command(subcommand)]
        sub: RelayDraftCommands,
    },
    /// List relays
    List {
        /// all (default), mine or sent
        #[arg(long)]
        perspective: Option<String>,
        /// published, acknowledged or closed (repeatable)
        #[arg(long = "state")]
        states: Vec<String>,
        #[arg(long)]
        workstream: Option<String>,
        #[command(flatten)]
        page: PageFlags,
    },
    /// Publish a local draft (captures branch/HEAD)
    Publish {
        #[arg(required_unless_present_any = ["apply", "request"])]
        draft_id: Option<String>,
        /// Expected actor: must be the current member (refused otherwise)
        #[arg(long)]
        member: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Show one relay
    Show {
        relay_id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Claim a published relay
    Acknowledge {
        #[arg(required_unless_present_any = ["apply", "request"])]
        relay_id: Option<String>,
        /// Expected actor: must be the current member (refused otherwise)
        #[arg(long)]
        member: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Close an acknowledged relay (sender or claimant)
    Close {
        #[arg(required_unless_present_any = ["apply", "request"])]
        relay_id: Option<String>,
        /// Expected actor: must be the current member (refused otherwise)
        #[arg(long)]
        member: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
}

fn actor_of(member: Option<String>) -> ActorChoice {
    match member {
        Some(m) => ActorChoice::member(&m),
        None => ActorChoice::resolved(),
    }
}

fn print_relay(r: &Value) {
    println!("# {} ({})", s(r, "title").bold(), s(r, "id"));
    println!("Status: {} | Sender: {}", s(r, "status").cyan(), s(r, "sender"));
    if r["openToTeam"].as_bool().unwrap_or(false) {
        println!("Recipients: open to team");
    } else {
        let names: Vec<String> = r["namedRecipients"].as_array().cloned().unwrap_or_default().iter().filter_map(|x| x.as_str().map(String::from)).collect();
        println!("Recipients: {}", names.join(", "));
    }
    if let Some(c) = r.get("claimant").and_then(|c| c.as_str()) {
        println!("Acknowledged by: {}", c);
    }
    if let Some(c) = r.get("closedBy").and_then(|c| c.as_str()) {
        println!("Closed by: {}", c);
    }
    let o = &r["observedState"];
    println!(
        "Observed: branch {} @ {}{}",
        o.get("branch").and_then(|x| x.as_str()).unwrap_or("-"),
        o.get("headCommit").and_then(|x| x.as_str()).unwrap_or("-"),
        if o["dirtyTree"].as_bool().unwrap_or(false) { " (dirty)" } else { "" }
    );
    println!("\n{}", s(r, "summary"));
    for (label, key) in [
        ("Completed", "completed"),
        ("In progress", "inProgress"),
        ("Progress", "progress"),
        ("Decisions", "decisions"),
        ("Blockers", "blockers"),
        ("Unresolved questions", "unresolvedQuestions"),
        ("Next actions", "nextActions"),
        ("Changed files", "changedFiles"),
        ("Evidence", "evidence"),
    ] {
        let items: Vec<String> = r[key].as_array().cloned().unwrap_or_default().iter().filter_map(|x| x.as_str().map(String::from)).collect();
        if !items.is_empty() {
            println!("\n{}:", label.bold());
            for i in items {
                println!("  - {}", i);
            }
        }
    }
    let code: Vec<Value> = r["code"].as_array().cloned().unwrap_or_default();
    if !code.is_empty() {
        println!("\n{}:", "Code".bold());
        for c in code {
            println!("  - {}", c.get("symbolId").or_else(|| c.get("path")).and_then(|x| x.as_str()).unwrap_or(""));
        }
    }
}

pub fn run_relay(config: &KnobyteConfig, cmd: RelayCommands) -> i32 {
    match cmd {
        RelayCommands::Contract { action, out } => run_read("relay.contract", out.json, || Ok((relay_contract(action.as_deref())?, Vec::new())), |d| {
            println!("{}", serde_json::to_string_pretty(d).unwrap_or_default());
        }),
        RelayCommands::Draft { sub } => match sub {
            RelayDraftCommands::List { page } => run_read("relay.draft.list", page.json, || {
                Ok((to_value(&list_relay_drafts_page(config, page.cursor.as_deref(), page.limit)?), Vec::new()))
            }, |d| {
                let items = d["items"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("No local relay drafts found.");
                }
                for x in items {
                    println!("- {} ({}) - {}", s(&x, "title").bold(), s(&x, "id"), s(&x, "summary"));
                }
                page_continuation(d);
            }),
            RelayDraftCommands::Show { id, out } => run_read("relay.draft.show", out.json, || {
                let d = get_relay_draft(config, &id).ok_or_else(|| TeamError::not_found("Relay draft", &id))?;
                Ok((to_value(&d), Vec::new()))
            }, print_relay),
            RelayDraftCommands::Save { draft_id, from, sender, fields, flags } => {
                run_mutation(config, "relay.draft.save", "relay.draft.save", &flags, actor_of(sender), || {
                    let input = match &from {
                        Some(path) => {
                            let mut v = read_bounded_json(path)?;
                            if !v.is_object() {
                                return Err(TeamError::usage("--from must contain a JSON object"));
                            }
                            if v.get("changedFiles").is_none() && !fields.no_auto_files {
                                v["changedFiles"] = json!(detect_changed_files(&config.project_root));
                            }
                            v
                        }
                        None => fields.to_input(config)?,
                    };
                    let mut a = json!({ "kind": "relay.draft.save", "draft": input });
                    if let Some(id) = draft_id {
                        a["draftId"] = json!(id);
                    }
                    Ok(a)
                }, |r| println!("{} Saved relay draft: {}", "[ok]".green().bold(), s(&r.result, "id")))
            }
            RelayDraftCommands::Delete { id, flags } => run_mutation(config, "relay.draft.delete", "relay.draft.delete", &flags, ActorChoice::resolved(), || {
                Ok(json!({ "kind": "relay.draft.delete", "draftId": required(id, "<draft-id>")? }))
            }, ok_line),
        },
        RelayCommands::List { perspective, states, workstream, page } => run_read("relay.list", page.json, || {
            Ok((to_value(&list_relays_page(config, perspective.as_deref(), &states, workstream.as_deref(), page.cursor.as_deref(), page.limit)?), Vec::new()))
        }, |d| {
            let items = d["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("No relays found.");
            }
            for r in items {
                println!("- [{}] {} ({}) by {}", s(&r, "status").cyan(), s(&r, "title").bold(), s(&r, "id"), s(&r, "sender"));
            }
            page_continuation(d);
        }),
        RelayCommands::Publish { draft_id, member, flags } => run_mutation(config, "relay.publish", "relay.publish", &flags, actor_of(member), || {
            Ok(json!({ "kind": "relay.publish", "draftId": required(draft_id, "<draft-id>")? }))
        }, |r| println!("{} Published relay '{}': {}", "[ok]".green().bold(), s(&r.result, "id"), s(&r.result, "title"))),
        RelayCommands::Show { relay_id, out } => run_read("relay.show", out.json, || {
            let r = get_relay(config, &relay_id).ok_or_else(|| TeamError::not_found("Relay", &relay_id))?;
            Ok((to_value(&r), Vec::new()))
        }, print_relay),
        RelayCommands::Acknowledge { relay_id, member, flags } => run_mutation(config, "relay.acknowledge", "relay.acknowledge", &flags, actor_of(member), || {
            Ok(json!({ "kind": "relay.acknowledge", "relayId": required(relay_id, "<relay-id>")? }))
        }, |r| println!("{} Claimed relay '{}' by {}", "[ok]".green().bold(), s(&r.result, "id"), s(&r.result, "claimant"))),
        RelayCommands::Close { relay_id, member, flags } => run_mutation(config, "relay.close", "relay.close", &flags, actor_of(member), || {
            Ok(json!({ "kind": "relay.close", "relayId": required(relay_id, "<relay-id>")? }))
        }, |r| println!("{} Closed relay '{}'", "[ok]".green().bold(), s(&r.result, "id"))),
    }
}

// ---------------------------------------------------------------------------
// log / timeline
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct LogArgs {
    pub message: String,
    /// Event kind: decision, discovery, note, risk, todo
    #[arg(long, alias = "type", default_value = "note")]
    pub kind: String,
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Related file path (repeatable)
    #[arg(long = "file")]
    pub files: Vec<String>,
    /// Where the event came from (e.g. meeting, manual, agent)
    #[arg(long)]
    pub source: Option<String>,
    /// Lifecycle status (e.g. decided, implemented)
    #[arg(long)]
    pub status: Option<String>,
}

#[derive(Args, Debug)]
pub struct TimelineArgs {
    /// Case-insensitive text in the summary, tags or details
    #[arg(long)]
    pub query: Option<String>,
    /// Filter by event kind: decision, discovery, note, risk, todo
    #[arg(long, alias = "type")]
    pub kind: Option<String>,
    /// Exact recorded file path (project-relative); any may match (repeatable, max 16)
    #[arg(long = "file")]
    pub files: Vec<String>,
    /// From YYYY-MM-DD, RFC 3339, or relative Nd such as 30d
    #[arg(long)]
    pub since: Option<String>,
    /// Maximum entries, 1-200
    #[arg(long, default_value_t = DEFAULT_TIMELINE_LIMIT)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
    /// Output format: md for a Markdown table
    #[arg(long)]
    pub format: Option<String>,
}

pub fn run_log(config: &KnobyteConfig, args: LogArgs) -> i32 {
    let usage = |msg: String| {
        eprintln!("{} {}", "[error]".red().bold(), msg);
        2
    };
    let kind = match normalize_event_kind(&args.kind) {
        Ok(k) => k,
        Err(e) => return usage(e),
    };
    if args.message.trim().is_empty() {
        return usage("Event message must not be empty".to_string());
    }
    if args.files.iter().any(|f| f.trim().is_empty()) {
        return usage("--file must not be empty".to_string());
    }
    let actor_label = crate::events::logging_actor(config);
    match append_logged_event(config, &args.message, &kind, &args.tags, &args.files, actor_label.as_deref(), args.source.as_deref(), args.status.as_deref()) {
        Ok(entry) => {
            println!("{} Logged {}: {}", "[ok]".green().bold(), entry.kind.cyan(), entry.summary);
            0
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => usage(e.to_string()),
        Err(e) => {
            eprintln!("{} {}", "[error]".red().bold(), e);
            1
        }
    }
}

pub fn run_timeline(config: &KnobyteConfig, args: TimelineArgs) -> i32 {
    let result = (|| -> Result<_, String> {
        let limit = validate_timeline_limit(args.limit)?;
        let since = args.since.as_deref().map(parse_timeline_since).transpose()?;
        if let Some(f) = &args.format {
            if f != "md" {
                return Err(format!("Unsupported --format '{}': only 'md' is supported", f));
            }
        }
        let query = args.query.as_deref().map(validate_timeline_query).transpose()?;
        let kind = args.kind.as_deref().map(normalize_event_kind).transpose()?;
        let files = validate_timeline_files(&config.project_root, &args.files)?;
        let filter = TimelineFilter { query, kind, file: None, since, include_superseded: false, limit };
        Ok(query_timeline_files(config, filter, &files))
    })();
    match result {
        Ok(resp) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_default());
            } else if args.format.as_deref() == Some("md") {
                print!("{}", render_timeline_markdown(&resp));
            } else {
                if resp.entries.is_empty() {
                    println!("No events found.");
                }
                for e in &resp.entries {
                    let files = if e.files.is_empty() { String::new() } else { format!(" ({})", e.files.join(", ")).dimmed().to_string() };
                    println!("[{}] {} - {}{}", e.timestamp.dimmed(), e.kind.cyan(), e.summary.bold(), files);
                }
                if resp.truncated {
                    println!("{}", "Some matching events were omitted by the entry limit; narrow the filters.".dimmed());
                }
            }
            0
        }
        Err(e) => {
            eprintln!("{} {}", "[error]".red().bold(), e);
            2
        }
    }
}

// ---------------------------------------------------------------------------
// playbook
// ---------------------------------------------------------------------------

#[derive(Args, Debug, Clone, Default)]
pub struct PlaybookFields {
    #[arg(long)]
    pub summary: Option<String>,
    /// When to use this playbook
    #[arg(long)]
    pub trigger: Option<String>,
    /// draft or active (publishing = setting active)
    #[arg(long)]
    pub state: Option<String>,
    /// Owner member id (repeatable)
    #[arg(long = "owner")]
    pub owners: Vec<String>,
    #[arg(long = "topic")]
    pub topics: Vec<String>,
    #[arg(long = "prerequisite")]
    pub prerequisites: Vec<String>,
    #[arg(long = "related")]
    pub related: Vec<String>,
    /// Step (repeatable, in order): "<title>[::<description>[::<evidence>;<evidence>...]]"
    #[arg(long = "step", value_name = "STEP")]
    pub steps: Vec<String>,
}

/// Parse `--step "<title>[::<description>[::<evidence>;...]]"`.
fn parse_step(spec: &str) -> Result<Value, TeamError> {
    let mut parts = spec.splitn(3, "::").map(str::trim);
    let title = parts.next().unwrap_or("");
    if title.is_empty() {
        return Err(TeamError::usage("--step needs a title: \"<title>[::<description>[::<evidence>;...]]\""));
    }
    let description = parts.next().unwrap_or("");
    let evidence: Vec<&str> = parts.next().unwrap_or("").split(';').map(str::trim).filter(|e| !e.is_empty()).collect();
    let mut v = json!({ "title": title });
    if !description.is_empty() {
        v["description"] = json!(description);
    }
    if !evidence.is_empty() {
        v["expectedEvidence"] = json!(evidence);
    }
    Ok(v)
}

fn parse_steps(specs: &[String]) -> Result<Vec<Value>, TeamError> {
    specs.iter().map(|s| parse_step(s)).collect()
}

#[derive(Subcommand, Debug)]
pub enum PlaybookCommands {
    /// Versioned JSON Schema catalog of playbook request files
    Contract {
        /// Only this action (e.g. playbook.run.complete-step)
        #[arg(long)]
        action: Option<String>,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// List playbooks (archived hidden unless requested)
    List {
        /// Filter by state: draft, active, archived (repeatable)
        #[arg(long = "state")]
        states: Vec<String>,
        #[arg(long)]
        topic: Option<String>,
        #[arg(long = "include-archived")]
        include_archived: bool,
        #[command(flatten)]
        page: PageFlags,
    },
    /// Show a playbook with its steps and recent runs
    Show {
        id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Create a playbook (draft unless --state active)
    Create {
        #[arg(required_unless_present_any = ["apply", "request"])]
        title: Option<String>,
        /// Explicit id (default: derived from the title)
        #[arg(long)]
        id: Option<String>,
        #[command(flatten)]
        fields: PlaybookFields,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Update a playbook; --step replaces the whole step list, --state active publishes a draft
    Update {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[command(flatten)]
        fields: PlaybookFields,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Archive a playbook (archived playbooks are immutable and cannot be run)
    Archive {
        #[arg(required_unless_present_any = ["apply", "request"])]
        id: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Start, inspect and advance playbook runs
    Run {
        #[command(subcommand)]
        sub: PlaybookRunCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum PlaybookRunCommands {
    /// Start a run of an active playbook (its steps are snapshotted)
    Start {
        #[arg(required_unless_present_any = ["apply", "request"])]
        playbook_id: Option<String>,
        /// Link the run to a workstream
        #[arg(long)]
        workstream: Option<String>,
        /// Optional label for this run
        #[arg(long)]
        title: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// List runs, newest first
    List {
        #[arg(long)]
        playbook: Option<String>,
        #[arg(long)]
        workstream: Option<String>,
        /// active, completed or abandoned (repeatable)
        #[arg(long = "state")]
        states: Vec<String>,
        #[command(flatten)]
        page: PageFlags,
    },
    /// Show one run with its step states and evidence
    Show {
        run_id: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Complete one pending step, recording evidence (the run completes with its last step)
    CompleteStep {
        #[arg(required_unless_present_any = ["apply", "request"])]
        run_id: Option<String>,
        /// Step id, or the step's 1-based number in the run
        #[arg(required_unless_present_any = ["apply", "request"])]
        step_id: Option<String>,
        /// Evidence: file:<path>, commit:<sha>, entity:<id>, code:<symbol>, a URL, or free text (repeatable)
        #[arg(long = "evidence")]
        evidence: Vec<String>,
        #[arg(long)]
        note: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Abandon an active run
    Abandon {
        #[arg(required_unless_present_any = ["apply", "request"])]
        run_id: Option<String>,
        #[arg(long, required_unless_present_any = ["apply", "request"])]
        reason: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
}

fn print_run(r: &Value) {
    let label = if s(r, "title").is_empty() { s(r, "playbookTitle") } else { s(r, "title") };
    println!("# {} ({}) [{}]", label.bold(), s(r, "id"), s(r, "state"));
    println!("Playbook: {} (revision {})", s(r, "playbookId"), r["playbookRevision"]);
    if let Some(w) = r.get("workstream").and_then(|w| w.as_str()) {
        println!("Workstream: {}", w);
    }
    println!("Started by {} at {}", s(r, "startedBy"), s(r, "startedAt"));
    if let Some(reason) = r.get("abandonReason").and_then(|x| x.as_str()) {
        println!("Abandoned by {}: {}", s(r, "abandonedBy"), reason);
    }
    for st in r["steps"].as_array().cloned().unwrap_or_default() {
        let done = s(&st, "state") == "completed";
        let mark = if done { "[x]".green().to_string() } else { "[ ]".to_string() };
        println!("  {} {} ({})", mark, s(&st, "title"), s(&st, "stepId"));
        if done {
            println!("      by {} at {}", s(&st, "completedBy"), s(&st, "completedAt"));
        } else {
            for e in st["expectedEvidence"].as_array().cloned().unwrap_or_default() {
                println!("      expects: {}", e.as_str().unwrap_or(""));
            }
        }
        for e in st["evidence"].as_array().cloned().unwrap_or_default() {
            if let Ok(ev) = serde_json::from_value::<crate::team::refs::EvidenceRef>(e) {
                println!("      evidence: {}", ev.display());
            }
        }
        if !s(&st, "note").is_empty() {
            println!("      note: {}", s(&st, "note"));
        }
    }
}

fn print_playbook_detail(d: &Value) {
    let p = &d["playbook"];
    println!("# {} ({}) [{}] rev {}", s(p, "title").bold(), s(p, "id"), s(p, "state"), p["entityRevision"]);
    for (label, key) in [("Summary", "summary"), ("Trigger", "trigger")] {
        if !s(p, key).is_empty() {
            println!("{}: {}", label, s(p, key));
        }
    }
    for (label, key) in [("Owners", "owners"), ("Topics", "topics"), ("Prerequisites", "prerequisites"), ("Related", "related")] {
        let items: Vec<String> = p[key].as_array().cloned().unwrap_or_default().iter().filter_map(|x| x.as_str().map(String::from)).collect();
        if !items.is_empty() {
            println!("{}: {}", label, items.join(", "));
        }
    }
    for (i, st) in p["steps"].as_array().cloned().unwrap_or_default().iter().enumerate() {
        println!("  {}. {} ({})", i + 1, s(st, "title").bold(), s(st, "id"));
        if !s(st, "description").is_empty() {
            println!("     {}", s(st, "description"));
        }
        for c in st["requiredChecks"].as_array().cloned().unwrap_or_default() {
            println!("     check: {}", c.as_str().unwrap_or(""));
        }
        for e in st["expectedEvidence"].as_array().cloned().unwrap_or_default() {
            println!("     evidence: {}", e.as_str().unwrap_or(""));
        }
    }
    let runs = d["runs"].as_array().cloned().unwrap_or_default();
    if !runs.is_empty() {
        println!("Runs:");
        for r in runs {
            println!("  - {} [{}] {}/{} steps, started {} by {}", s(&r, "id"), s(&r, "state"), r["stepsCompleted"], r["stepsTotal"], s(&r, "startedAt"), s(&r, "startedBy"));
        }
    }
}

pub fn run_playbook(config: &KnobyteConfig, cmd: PlaybookCommands) -> i32 {
    use crate::team::playbooks::{list_playbooks_page, playbook_detail, PlaybookPatch, PlaybookStepInput};
    match cmd {
        PlaybookCommands::Contract { action, out } => {
            run_read("playbook.contract", out.json, || Ok((crate::team::contract::playbook_contract(action.as_deref())?, Vec::new())), print_contract)
        }
        PlaybookCommands::List { states, topic, include_archived, page } => run_read("playbook.list", page.json, || {
            Ok((to_value(&list_playbooks_page(config, &states, topic.as_deref(), include_archived, page.cursor.as_deref(), page.limit)?), Vec::new()))
        }, |d| {
            let items = d["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("No playbooks found.");
            }
            for p in items {
                println!("- {} ({}) [{}] {} step(s)", s(&p, "title").bold(), s(&p, "id"), s(&p, "state"), p["steps"].as_array().map(|a| a.len()).unwrap_or(0));
            }
            page_continuation(d);
        }),
        PlaybookCommands::Show { id, out } => run_read("playbook.show", out.json, || Ok((playbook_detail(config, &id)?, Vec::new())), print_playbook_detail),
        PlaybookCommands::Create { title, id, fields, flags } => run_mutation(config, "playbook.create", "playbook.create", &flags, ActorChoice::resolved(), || {
            Ok(json!({ "kind": "playbook.create", "playbook": {
                "id": id,
                "title": required(title, "<title>")?,
                "summary": fields.summary,
                "trigger": fields.trigger,
                "state": fields.state,
                "owners": fields.owners,
                "topics": fields.topics,
                "prerequisites": fields.prerequisites,
                "related": fields.related,
                "steps": parse_steps(&fields.steps)?,
            }}))
        }, ok_line),
        PlaybookCommands::Update { id, title, fields, flags } => run_mutation(config, "playbook.update", "playbook.update", &flags, ActorChoice::resolved(), || {
            let steps = if fields.steps.is_empty() {
                None
            } else {
                let parsed = parse_steps(&fields.steps)?;
                Some(
                    parsed
                        .into_iter()
                        .map(|v| serde_json::from_value::<PlaybookStepInput>(v).map_err(|e| TeamError::usage(e.to_string())))
                        .collect::<Result<Vec<_>, _>>()?,
                )
            };
            let patch = PlaybookPatch {
                title,
                summary: fields.summary,
                trigger: fields.trigger,
                state: fields.state,
                owners: opt_list(fields.owners),
                topics: opt_list(fields.topics),
                prerequisites: opt_list(fields.prerequisites),
                related: opt_list(fields.related),
                steps,
            };
            Ok(json!({ "kind": "playbook.update", "playbookId": required(id, "<id>")?, "patch": patch }))
        }, ok_line),
        PlaybookCommands::Archive { id, flags } => run_mutation(config, "playbook.archive", "playbook.archive", &flags, ActorChoice::resolved(), || {
            Ok(json!({ "kind": "playbook.archive", "playbookId": required(id, "<id>")? }))
        }, ok_line),
        PlaybookCommands::Run { sub } => run_playbook_run(config, sub),
    }
}

fn run_playbook_run(config: &KnobyteConfig, cmd: PlaybookRunCommands) -> i32 {
    use crate::team::playbooks::{get_run, list_runs_page, run_summary, RunFilter};
    match cmd {
        PlaybookRunCommands::Start { playbook_id, workstream, title, flags } => run_mutation(config, "playbook.run.start", "playbook.run.start", &flags, ActorChoice::resolved(), || {
            Ok(json!({ "kind": "playbook.run.start", "playbookId": required(playbook_id, "<playbook-id>")?, "workstream": workstream, "title": title }))
        }, |r| println!("{} {}", "[ok]".green().bold(), r.summary)),
        PlaybookRunCommands::List { playbook, workstream, states, page } => run_read("playbook.run.list", page.json, || {
            let f = RunFilter { playbook, workstream, states };
            let p = list_runs_page(config, &f, page.cursor.as_deref(), page.limit)?;
            let items: Vec<Value> = p.items.iter().map(run_summary).collect();
            Ok((json!({ "items": items, "nextCursor": p.next_cursor, "truncated": p.truncated, "total": p.total, "deterministicRevision": p.deterministic_revision }), Vec::new()))
        }, |d| {
            let items = d["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("No playbook runs found.");
            }
            for r in items {
                println!("- {} [{}] {} {}/{} steps, started {} by {}", s(&r, "id"), s(&r, "state").cyan(), s(&r, "playbookTitle").bold(), r["stepsCompleted"], r["stepsTotal"], s(&r, "startedAt"), s(&r, "startedBy"));
            }
            page_continuation(d);
        }),
        PlaybookRunCommands::Show { run_id, out } => run_read("playbook.run.show", out.json, || {
            let r = get_run(config, &run_id).ok_or_else(|| TeamError::not_found("Playbook run", &run_id))?;
            Ok((to_value(&r), Vec::new()))
        }, print_run),
        PlaybookRunCommands::CompleteStep { run_id, step_id, evidence, note, flags } => run_mutation(config, "playbook.run.complete-step", "playbook.run.complete-step", &flags, ActorChoice::resolved(), || {
            let evidence = parse_evidence(&evidence).map_err(TeamError::usage)?;
            Ok(json!({ "kind": "playbook.run.complete-step", "runId": required(run_id, "<run-id>")?, "stepId": required(step_id, "<step-id>")?, "evidence": evidence, "note": note }))
        }, |r| {
            println!("{} {}", "[ok]".green().bold(), r.summary);
            let pending = r.result["steps"].as_array().map(|a| a.iter().filter(|x| x["state"] != "completed").count()).unwrap_or(0);
            if pending > 0 {
                println!("{} step(s) remaining.", pending);
            }
        }),
        PlaybookRunCommands::Abandon { run_id, reason, flags } => run_mutation(config, "playbook.run.abandon", "playbook.run.abandon", &flags, ActorChoice::resolved(), || {
            Ok(json!({ "kind": "playbook.run.abandon", "runId": required(run_id, "<run-id>")?, "reason": required(reason, "--reason")? }))
        }, |r| println!("{} {}", "[ok]".green().bold(), r.summary)),
    }
}

// ---------------------------------------------------------------------------
// catch-up
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct CatchUpArgs {
    #[command(subcommand)]
    pub sub: Option<CatchUpCommands>,
    /// Override the baseline: RFC 3339, YYYY-MM-DD, or relative Nd/Nh (default: your cursor, else 7d)
    #[arg(long)]
    pub since: Option<String>,
    /// Only items related to this workstream
    #[arg(long)]
    pub workstream: Option<String>,
    /// Only these groups: handoffs, reviews, decisions, knowledge, workstreams, playbooks, activity (repeatable)
    #[arg(long = "group")]
    pub groups: Vec<String>,
    /// Also list your own changes
    #[arg(long = "include-mine")]
    pub include_mine: bool,
    #[command(flatten)]
    pub page: PageFlags,
}

#[derive(Subcommand, Debug)]
pub enum CatchUpCommands {
    /// Mark everything up to now (or --at) as seen: advances your checkout-local cursor
    Mark {
        /// RFC 3339 instant to mark up to (e.g. the digest's observedAt)
        #[arg(long)]
        at: Option<String>,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Reset your cursor (also adopts the current branch); --clear removes it
    Reset {
        /// New baseline: RFC 3339, YYYY-MM-DD, or relative Nd/Nh (default: now)
        #[arg(long, conflicts_with = "clear")]
        to: Option<String>,
        #[arg(long)]
        clear: bool,
        #[command(flatten)]
        flags: MutationFlags,
    },
    /// Versioned JSON Schema catalog of catch-up request files
    Contract {
        /// Only this action (catchup.mark or catchup.reset)
        #[arg(long)]
        action: Option<String>,
        #[command(flatten)]
        out: JsonFlag,
    },
}

fn print_digest(d: &Value) {
    use crate::team::catchup::CATCH_UP_GROUPS;
    let source = match s(d, "baselineSource").as_str() {
        "cursor" => "your last catch-up",
        "since" => "--since",
        _ => "default window; no cursor yet",
    };
    println!("{} for {} since {} ({})", "Catch up".bold(), s(d, "actorId"), s(d, "baseline"), source);
    let items = d["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("You are all caught up.");
    }
    for g in CATCH_UP_GROUPS {
        let in_group: Vec<&Value> = items.iter().filter(|i| i["group"] == *g).collect();
        if in_group.is_empty() {
            continue;
        }
        println!("\n{} ({})", g.to_uppercase().bold(), d["groups"][*g]);
        for i in in_group {
            let open = if i["new"].as_bool().unwrap_or(false) { "" } else { " (still open)" };
            println!("  [{}] {}{}", s(i, "occurredAt").dimmed(), s(i, "title").bold(), open);
            println!("      {}", s(i, "summary"));
        }
    }
    page_continuation(d);
    if !items.is_empty() {
        println!("\nRun `knobyte catch-up mark --at {}` once you have read this.", s(d, "observedAt"));
    }
}

pub fn run_catch_up(config: &KnobyteConfig, args: CatchUpArgs) -> i32 {
    use crate::team::catchup::{catch_up_digest, CatchUpRequest};
    match args.sub {
        Some(CatchUpCommands::Mark { at, flags }) => run_mutation(config, "catchup.mark", "catchup.mark", &flags, ActorChoice::resolved(), || {
            Ok(json!({ "kind": "catchup.mark", "at": at }))
        }, |r| println!("{} Caught up to {}", "[ok]".green().bold(), s(&r.result, "timestamp"))),
        Some(CatchUpCommands::Reset { to, clear, flags }) => run_mutation(config, "catchup.reset", "catchup.reset", &flags, ActorChoice::resolved(), || {
            Ok(json!({ "kind": "catchup.reset", "to": to, "clear": clear }))
        }, |r| println!("{} {}", "[ok]".green().bold(), r.summary)),
        Some(CatchUpCommands::Contract { action, out }) => {
            run_read("catchup.contract", out.json, || Ok((crate::team::contract::catchup_contract(action.as_deref())?, Vec::new())), print_contract)
        }
        None => {
            let req = CatchUpRequest {
                since: args.since,
                workstream: args.workstream,
                include_mine: args.include_mine,
                groups: args.groups,
                cursor: args.page.cursor.clone(),
                limit: args.page.limit,
            };
            run_read("catchup.digest", args.page.json, || catch_up_digest(config, &req), print_digest)
        }
    }
}
