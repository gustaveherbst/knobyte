//! MCP tool profiles and legacy tool-name aliases.
//!
//! A profile selects which tools `tools/list` advertises (and `tools/call` accepts). The
//! default, `core`, is the focused day-to-day set; `team`, `wiki` and `graph` add the tools of
//! one area on top of it; `full` is every tool. [`TOOL_PROFILES`] is the single table that
//! assigns tools to profiles. [`ALIASES`] lists retired tool names that still answer
//! `tools/call` (never listed) by mapping to their merged tool.
//!
//! The active profile is chosen by precedence: `--profile` flag > `KNOBYTE_MCP_PROFILE` >
//! `.knobyte/config.json` `mcp.profile` > `core`.

use std::fmt;
use std::path::Path;

use serde::Serialize;

/// Environment variable selecting the MCP tool profile.
pub const PROFILE_ENV_VAR: &str = "KNOBYTE_MCP_PROFILE";

/// A named subset of the MCP tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpProfile {
    /// Focused day-to-day set: context, code search, wiki reading, checks, log, handoffs.
    #[default]
    Core,
    /// Core plus playbooks, relays and the rest of the team workflow.
    Team,
    /// Core plus wiki authoring and validation.
    Wiki,
    /// Core plus Datalog, PageRank, shortest path and raw graph access.
    Graph,
    /// Every tool.
    Full,
}

use McpProfile::{Core, Graph, Team, Wiki};

/// Profiles below `full` that include the core tools.
const ALL_BUT_FULL: &[McpProfile] = &[Core, Team, Wiki, Graph];

/// Which profiles list each tool (`full` lists every tool and is implied). One row per tool,
/// in `tools/list` order.
pub const TOOL_PROFILES: &[(&str, &[McpProfile])] = &[
    // core: what an agent needs day to day (also part of team, wiki and graph)
    ("knobyte_session_start", ALL_BUT_FULL),
    ("knobyte_graph_scope", ALL_BUT_FULL),
    ("knobyte_graph_query", ALL_BUT_FULL),
    ("knobyte_vector_search", ALL_BUT_FULL),
    ("knobyte_wiki_search", ALL_BUT_FULL),
    ("knobyte_wiki_get", ALL_BUT_FULL),
    ("knobyte_file_context", ALL_BUT_FULL),
    ("knobyte_check", ALL_BUT_FULL),
    ("knobyte_log", ALL_BUT_FULL),
    ("knobyte_timeline", ALL_BUT_FULL),
    ("knobyte_catch_up", ALL_BUT_FULL),
    ("knobyte_members", ALL_BUT_FULL),
    ("knobyte_workstream_step_update", ALL_BUT_FULL),
    ("knobyte_relay_draft", ALL_BUT_FULL),
    ("knobyte_inbox_draft", ALL_BUT_FULL),
    // team
    ("knobyte_relay_list", &[Team]),
    ("knobyte_playbooks", &[Team]),
    ("knobyte_playbook_complete_step", &[Team]),
    // wiki
    ("knobyte_wiki_neighborhood", &[Wiki]),
    ("knobyte_wiki_validate", &[Wiki]),
    ("knobyte_wiki_plan_operation", &[Wiki]),
    ("knobyte_wiki_apply_operation", &[Wiki]),
    // graph
    ("knobyte_graph_get", &[Graph]),
    ("knobyte_graph_status", &[Graph]),
    ("knobyte_cozo_datalog", &[Graph]),
    ("knobyte_cozo_pagerank", &[Graph]),
    ("knobyte_cozo_shortest_path", &[Graph]),
    // full only
    ("knobyte_heartbeat", &[]),
    ("knobyte_read_file", &[]),
    ("knobyte_harvest", &[]),
];

/// A retired tool name that still answers `tools/call` (it is never listed).
#[derive(Debug, Clone, Copy)]
pub struct ToolAlias {
    /// The retired name.
    pub name: &'static str,
    /// The merged tool that now provides the behaviour (decides profile membership).
    pub target: &'static str,
    /// How to get the same result from the merged tool.
    pub equivalent: &'static str,
}

/// Retired tool names. Each keeps its arguments and result shape.
pub const ALIASES: &[ToolAlias] = &[
    ToolAlias { name: "knobyte_wiki_show", target: "knobyte_wiki_get", equivalent: "knobyte_wiki_get with includeBody: true" },
    ToolAlias { name: "knobyte_wiki_grounding_status", target: "knobyte_wiki_get", equivalent: "knobyte_wiki_get (groundings field)" },
    ToolAlias { name: "knobyte_wiki_list", target: "knobyte_wiki_search", equivalent: "knobyte_wiki_search without query" },
    ToolAlias { name: "knobyte_wiki_query", target: "knobyte_wiki_search", equivalent: "knobyte_wiki_search with query" },
    ToolAlias { name: "knobyte_member_list", target: "knobyte_members", equivalent: "knobyte_members (members field)" },
    ToolAlias { name: "knobyte_member_current", target: "knobyte_members", equivalent: "knobyte_members (current field)" },
    ToolAlias { name: "knobyte_playbook_list", target: "knobyte_playbooks", equivalent: "knobyte_playbooks without id/runId" },
    ToolAlias { name: "knobyte_playbook_get", target: "knobyte_playbooks", equivalent: "knobyte_playbooks with id or runId" },
    ToolAlias { name: "knobyte_catch_up_mark", target: "knobyte_catch_up", equivalent: "knobyte_catch_up with mark: true" },
    ToolAlias { name: "knobyte_sync_groundings", target: "knobyte_check", equivalent: "knobyte_check with fix: true (dryRun to preview)" },
];

/// The alias entry for a retired tool name.
pub fn alias(name: &str) -> Option<&'static ToolAlias> {
    ALIASES.iter().find(|a| a.name == name)
}

/// The tool that decides profile membership for `name` (aliases resolve to their target).
pub fn canonical_tool(name: &str) -> &str {
    alias(name).map(|a| a.target).unwrap_or(name)
}

impl McpProfile {
    /// Every profile, smallest first.
    pub const ALL: [McpProfile; 5] = [McpProfile::Core, McpProfile::Team, McpProfile::Wiki, McpProfile::Graph, McpProfile::Full];

    pub fn name(self) -> &'static str {
        match self {
            McpProfile::Core => "core",
            McpProfile::Team => "team",
            McpProfile::Wiki => "wiki",
            McpProfile::Graph => "graph",
            McpProfile::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        McpProfile::ALL.into_iter().find(|p| p.name() == s)
    }

    /// One-line description of the profile.
    pub fn summary(self) -> &'static str {
        match self {
            McpProfile::Core => "day-to-day set: context, code and wiki search, checks, log, catch-up, drafts",
            McpProfile::Team => "core plus relays and playbooks",
            McpProfile::Wiki => "core plus wiki authoring, validation and neighbourhood",
            McpProfile::Graph => "core plus Datalog, PageRank, shortest path and raw graph access",
            McpProfile::Full => "every tool",
        }
    }

    /// Whether this profile lists the (canonical) tool `name`.
    pub fn includes(self, name: &str) -> bool {
        TOOL_PROFILES
            .iter()
            .find(|(t, _)| *t == name)
            .is_some_and(|(_, profiles)| self == McpProfile::Full || profiles.contains(&self))
    }

    /// The tool names of this profile, in `tools/list` order.
    pub fn tool_names(self) -> Vec<&'static str> {
        TOOL_PROFILES
            .iter()
            .filter(|(t, _)| self.includes(t))
            .map(|(t, _)| *t)
            .collect()
    }

    /// The profile from env/config (no flag), falling back to the default on an invalid value.
    pub fn configured(scaffold_root: &Path) -> McpProfile {
        resolve_profile_for(None, scaffold_root).map(|r| r.profile).unwrap_or_default()
    }
}

impl fmt::Display for McpProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Every profile that lists the (canonical) tool `name`.
pub fn profiles_including(name: &str) -> Vec<McpProfile> {
    McpProfile::ALL.into_iter().filter(|p| p.includes(name)).collect()
}

/// Comma-separated profile names, for help text and errors.
pub fn profile_names() -> String {
    McpProfile::ALL.map(|p| p.name()).join(", ")
}

/// Where the active profile came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileSource {
    Flag,
    Env,
    Config,
    Default,
}

impl fmt::Display for ProfileSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ProfileSource::Flag => "--profile",
            ProfileSource::Env => PROFILE_ENV_VAR,
            ProfileSource::Config => ".knobyte/config.json mcp.profile",
            ProfileSource::Default => "default",
        })
    }
}

/// The active profile and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolvedProfile {
    pub profile: McpProfile,
    pub source: ProfileSource,
}

/// Pick the profile by precedence: flag > env > config > default. Blank values are skipped;
/// an unknown name is an error naming its source.
pub fn resolve_profile(flag: Option<&str>, env: Option<&str>, config: Option<&str>) -> Result<ResolvedProfile, String> {
    for (value, source) in [(flag, ProfileSource::Flag), (env, ProfileSource::Env), (config, ProfileSource::Config)] {
        if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
            return match McpProfile::parse(v) {
                Some(profile) => Ok(ResolvedProfile { profile, source }),
                None => Err(format!(
                    "Unknown MCP profile '{}' (from {}); expected one of: {}",
                    v,
                    source,
                    profile_names()
                )),
            };
        }
    }
    Ok(ResolvedProfile { profile: McpProfile::default(), source: ProfileSource::Default })
}

/// `mcp.profile` from `<scaffold_root>/config.json` (nested `{"mcp": {"profile": ...}}`).
pub fn config_profile(scaffold_root: &Path) -> Option<String> {
    let value = crate::config::read_config_value(scaffold_root)?;
    value.get("mcp")?.get("profile")?.as_str().map(str::to_string)
}

/// Resolve the profile for a server: the flag, then `KNOBYTE_MCP_PROFILE`, then config.json.
pub fn resolve_profile_for(flag: Option<&str>, scaffold_root: &Path) -> Result<ResolvedProfile, String> {
    let env = std::env::var(PROFILE_ENV_VAR).ok();
    resolve_profile(flag, env.as_deref(), config_profile(scaffold_root).as_deref())
}

/// The `tools/call` error for a tool outside the active profile.
pub fn out_of_profile_message(name: &str, active: McpProfile) -> String {
    let canonical = canonical_tool(name);
    let profiles: Vec<&str> = profiles_including(canonical).into_iter().map(|p| p.name()).collect();
    let renamed = if canonical != name { format!(" (now {})", canonical) } else { String::new() };
    let suggest = profiles.first().copied().unwrap_or("full");
    format!(
        "Tool '{}'{} is not in the active MCP profile '{}'. It is available in profile(s): {}. Restart the server with `knobyte mcp --profile {}` (or set {} / mcp.profile in .knobyte/config.json).",
        name,
        renamed,
        active,
        profiles.join(", "),
        suggest,
        PROFILE_ENV_VAR
    )
}
