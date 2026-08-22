//! Actor resolution: configured member -> unique Git alias -> Git fallback -> unknown.

use std::fs;

use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;
use crate::team::envelope::Diagnostic;
use crate::team::members::{detect_git_user, get_member, list_members, CurrentMemberSelection, Member};

/// Who performed (or is performing) an action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ActorRef {
    Member {
        #[serde(rename = "memberId")]
        member_id: String,
        #[serde(default, rename = "displayName", skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    Git {
        name: Option<String>,
        email: Option<String>,
    },
    Unknown,
}

impl ActorRef {
    pub fn member(m: &Member) -> Self {
        ActorRef::Member { member_id: m.id.clone(), display_name: Some(m.display_name.clone()) }
    }

    /// Stable string form stored in legacy string fields (`author`, `actor`, `sender`, ...).
    pub fn id(&self) -> String {
        match self {
            ActorRef::Member { member_id, .. } => member_id.clone(),
            ActorRef::Git { name, email } => match (email, name) {
                (Some(e), _) => format!("git:{}", e),
                (None, Some(n)) => format!("git:{}", n),
                (None, None) => "unknown".to_string(),
            },
            ActorRef::Unknown => "unknown".to_string(),
        }
    }

    pub fn member_id(&self) -> Option<&str> {
        match self {
            ActorRef::Member { member_id, .. } => Some(member_id),
            _ => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            ActorRef::Member { member_id, display_name } => display_name.clone().unwrap_or_else(|| member_id.clone()),
            ActorRef::Git { name, email } => name.clone().or_else(|| email.clone()).unwrap_or_else(|| "unknown".to_string()),
            ActorRef::Unknown => "unknown".to_string(),
        }
    }

    /// Strip presentation-only fields so two refs to the same principal compare equal.
    pub fn principal(&self) -> ActorRef {
        match self {
            ActorRef::Member { member_id, .. } => ActorRef::Member { member_id: member_id.clone(), display_name: None },
            other => other.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ActorSource {
    ConfiguredMember,
    GitAlias,
    GitFallback,
    Unknown,
    /// Supplied explicitly by the caller (`--member`, Hub session).
    Explicit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorResolution {
    pub actor: ActorRef,
    pub source: ActorSource,
    pub selection: Option<CurrentMemberSelection>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn selection_path(config: &KnobyteConfig) -> std::path::PathBuf {
    config.local_dir().join("current_member.json")
}

pub fn read_selection(config: &KnobyteConfig) -> Option<CurrentMemberSelection> {
    let content = fs::read_to_string(selection_path(config)).ok()?;
    serde_json::from_str(&content).ok()
}

fn eq_ci(a: Option<&str>, b: &str) -> bool {
    a.map(|x| x.trim().eq_ignore_ascii_case(b.trim())).unwrap_or(false)
}

/// Active members whose email or Git alias matches the given Git identity.
pub fn members_matching_git(members: &[Member], name: Option<&str>, email: Option<&str>) -> Vec<Member> {
    let mut out: Vec<Member> = Vec::new();
    for m in members.iter().filter(|m| m.is_active()) {
        let email_hit = email
            .filter(|e| !e.trim().is_empty())
            .map(|e| eq_ci(m.email.as_deref(), e) || m.git_aliases.iter().any(|a| eq_ci(a.email.as_deref(), e)))
            .unwrap_or(false);
        let name_hit = name
            .filter(|n| !n.trim().is_empty())
            .map(|n| m.git_aliases.iter().any(|a| eq_ci(a.name.as_deref(), n)))
            .unwrap_or(false);
        if email_hit || name_hit {
            out.push(m.clone());
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Resolve the effective actor of this checkout. Pure read.
pub fn resolve_actor(config: &KnobyteConfig) -> ActorResolution {
    let selection = read_selection(config);
    let mut diagnostics = Vec::new();
    if let Some(sel) = &selection {
        match get_member(config, &sel.member_id) {
            Some(m) if m.is_active() => {
                return ActorResolution {
                    actor: ActorRef::member(&m),
                    source: ActorSource::ConfiguredMember,
                    selection,
                    diagnostics,
                }
            }
            Some(_) => diagnostics.push(Diagnostic::warning(
                "ACTOR_MEMBER_INACTIVE",
                format!("Configured member {} is inactive; clear the stale local selection.", sel.member_id),
            )),
            None => diagnostics.push(Diagnostic::warning(
                "ACTOR_MEMBER_MISSING",
                format!("Configured member {} no longer exists; clear the stale local selection.", sel.member_id),
            )),
        }
    }

    let (name, email) = detect_git_user(&config.project_root);
    if name.is_none() && email.is_none() {
        return ActorResolution { actor: ActorRef::Unknown, source: ActorSource::Unknown, selection, diagnostics };
    }
    let matches = members_matching_git(&list_members(config), name.as_deref(), email.as_deref());
    if matches.len() == 1 {
        return ActorResolution {
            actor: ActorRef::member(&matches[0]),
            source: ActorSource::GitAlias,
            selection,
            diagnostics,
        };
    }
    if matches.len() > 1 {
        diagnostics.push(Diagnostic::warning(
            "ACTOR_ALIAS_AMBIGUOUS",
            format!(
                "Git identity matches multiple active members ({}); select one with `knobyte member select <id>`.",
                matches.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", ")
            ),
        ));
    }
    ActorResolution { actor: ActorRef::Git { name, email }, source: ActorSource::GitFallback, selection, diagnostics }
}

/// Resolve an explicit member id (from `--member` or a Hub session) as the actor.
pub fn explicit_actor(config: &KnobyteConfig, member_id: &str) -> Result<ActorResolution, String> {
    let m = get_member(config, member_id).ok_or_else(|| format!("Member '{}' not found", member_id))?;
    if !m.is_active() {
        return Err(format!("Member '{}' is not active", member_id));
    }
    Ok(ActorResolution {
        actor: ActorRef::member(&m),
        source: ActorSource::Explicit,
        selection: read_selection(config),
        diagnostics: Vec::new(),
    })
}

/// String id of the current actor (member id, `git:<email>`, or `unknown`).
pub fn current_actor_id(config: &KnobyteConfig) -> String {
    resolve_actor(config).actor.id()
}
