//! Agent JSONL protocol v3 and its budgeted serializer.
//!
//! The agent-facing commands (`graph scope` / `query` / `get`, `impact`) stream
//! newline-delimited JSON. Every stream is framed by a `meta` record first and a `summary`
//! record last, with data records (`health`, `source`, `flow`, `fact`, `edge`, `result`,
//! `defines`, `caller`, `grounding`, `knowledge`, `error`) between. A hard output-token budget
//! is enforced while planning: every record is accounted (framing included), records that do
//! not fit are dropped in a deterministic priority order and the summary says what was
//! omitted.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub const AGENT_PROTOCOL_VERSION: u32 = 3;
const CHARS_PER_TOKEN: usize = 4;
/// Tokens reserved for the mandatory trailing `summary` record when no better estimate exists.
pub const FRAMING_RESERVE: usize = 140;
/// Stable worst-case width for numeric summary fields, so the reserve covers them.
const SIZE_PROBE: i64 = 9_999_999;
/// Slack over the anticipated summary size.
pub(crate) const RESERVE_PAD: usize = 16;

const SUMMARY_LIST_KEYS: [&str; 6] = [
    "sourceBackedNodes",
    "coveredTerms",
    "returnedFiles",
    "textFallbackFiles",
    "suggestedNextCommands",
    "warnings",
];
const SUMMARY_LIST_LIMITS: [usize; 6] = [24, 12, 6, 6, 3, 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DetailLevel {
    #[default]
    Minimal,
    Standard,
    Source,
}

impl DetailLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "minimal" => Some(Self::Minimal),
            "standard" => Some(Self::Standard),
            "source" => Some(Self::Source),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Standard => "standard",
            Self::Source => "source",
        }
    }
}

/// Retrieval controls shared by every agent command.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOptions {
    pub detail: DetailLevel,
    pub max_nodes: usize,
    pub max_files: usize,
    pub max_flow_steps: usize,
    pub max_output_tokens: usize,
    pub max_source_lines: usize,
    pub depth: usize,
    /// Attach the serialized fingerprint to each fact (grounding workflow).
    pub fingerprint: bool,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            detail: DetailLevel::Minimal,
            max_nodes: 10,
            max_files: 4,
            max_flow_steps: 8,
            max_output_tokens: 1500,
            max_source_lines: 120,
            depth: 2,
            fingerprint: false,
        }
    }
}

/// Raw (optional) options from the CLI or MCP; unset values take command defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentOptionsInput {
    pub detail: Option<DetailLevel>,
    pub max_nodes: Option<usize>,
    pub max_files: Option<usize>,
    pub max_flow_steps: Option<usize>,
    pub max_output_tokens: Option<usize>,
    pub max_source_lines: Option<usize>,
    pub depth: Option<usize>,
    pub fingerprint: bool,
}

impl AgentOptionsInput {
    /// True when any budget/detail control was given.
    pub fn any_set(&self) -> bool {
        self.detail.is_some()
            || self.max_nodes.is_some()
            || self.max_files.is_some()
            || self.max_flow_steps.is_some()
            || self.max_output_tokens.is_some()
            || self.max_source_lines.is_some()
            || self.fingerprint
    }

    /// Options for targeted commands (query, get, impact).
    pub fn resolve(&self) -> AgentOptions {
        let d = AgentOptions::default();
        AgentOptions {
            detail: self.detail.unwrap_or(d.detail),
            max_nodes: self.max_nodes.unwrap_or(d.max_nodes),
            max_files: self.max_files.unwrap_or(d.max_files),
            max_flow_steps: self.max_flow_steps.unwrap_or(d.max_flow_steps),
            max_output_tokens: self.max_output_tokens.unwrap_or(d.max_output_tokens),
            max_source_lines: self.max_source_lines.unwrap_or(d.max_source_lines),
            depth: self.depth.unwrap_or(d.depth),
            fingerprint: self.fingerprint,
        }
    }

    /// Adaptive one-call defaults used by broad scope retrieval (source-bearing by default).
    pub fn resolve_scope(&self, indexed_files: usize) -> AgentOptions {
        let (tokens, files) = if indexed_files < 150 {
            (3500, 4)
        } else if indexed_files < 500 {
            (4500, 5)
        } else {
            (6000, 6)
        };
        AgentOptionsInput {
            detail: Some(self.detail.unwrap_or(DetailLevel::Source)),
            max_nodes: Some(self.max_nodes.unwrap_or(24)),
            max_files: Some(self.max_files.unwrap_or(files)),
            max_flow_steps: Some(self.max_flow_steps.unwrap_or(8)),
            max_output_tokens: Some(self.max_output_tokens.unwrap_or(tokens)),
            max_source_lines: Some(self.max_source_lines.unwrap_or(200)),
            depth: self.depth,
            fingerprint: self.fingerprint,
        }
        .resolve()
    }
}

/// Deterministic, model-agnostic token estimate (serialized JSON length / 4, rounded up).
pub fn estimate_tokens(value: &Value) -> usize {
    let len = serde_json::to_string(value).map(|s| s.chars().count()).unwrap_or(0);
    len.div_ceil(CHARS_PER_TOKEN)
}

/// Plan-then-emit token accounting. Framing records are always counted; data records are
/// admitted only while they fit under the ceiling minus the summary reserve.
#[derive(Debug, Clone)]
pub struct BudgetLedger {
    used: usize,
    max: usize,
    reserve: usize,
    dropped: bool,
}

impl BudgetLedger {
    pub fn new(max_output_tokens: usize, reserve: usize) -> Self {
        Self {
            used: 0,
            max: max_output_tokens,
            reserve,
            dropped: false,
        }
    }

    /// Account a mandatory framing record (meta / summary).
    pub fn frame(&mut self, record: &Value) {
        self.used += estimate_tokens(record);
    }

    /// Whether a record would fit (no side effects).
    pub fn fits(&self, record: &Value) -> bool {
        self.used + estimate_tokens(record) <= self.max.saturating_sub(self.reserve)
    }

    /// Reserve budget for a data record; true iff it fits.
    pub fn try_add(&mut self, record: &Value) -> bool {
        if !self.fits(record) {
            self.dropped = true;
            return false;
        }
        self.used += estimate_tokens(record);
        true
    }

    /// Tokens still available to data records.
    pub fn available(&self) -> usize {
        self.max.saturating_sub(self.reserve).saturating_sub(self.used)
    }

    pub fn mark_dropped(&mut self) {
        self.dropped = true;
    }

    pub fn dropped_any(&self) -> bool {
        self.dropped
    }

    pub fn over_budget(&self) -> bool {
        self.used > self.max
    }

    pub fn estimated_tokens(&self) -> usize {
        self.used
    }
}

/// One framed response under construction.
pub struct Response {
    pub ledger: BudgetLedger,
    pub meta: Value,
    pub effective_max: usize,
    pub records: Vec<Value>,
}

/// A response that could not be framed: the requested ceiling is below mandatory framing.
pub fn budget_error(requested: usize, minimum: usize) -> Value {
    json!({
        "type": "error",
        "code": "INVALID_OUTPUT_BUDGET",
        "message": format!("--max-output-tokens {} cannot fit protocol framing; use at least {}.", requested, minimum),
        "minimum": minimum,
    })
}

pub fn meta_record(command: &str, opts: &AgentOptions, task: Option<&str>) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!("meta"));
    m.insert("protocolVersion".into(), json!(AGENT_PROTOCOL_VERSION));
    m.insert("schemaVersion".into(), json!(AGENT_PROTOCOL_VERSION));
    m.insert("command".into(), json!(command));
    if let Some(t) = task {
        m.insert("task".into(), json!(t));
    }
    m.insert("detail".into(), json!(opts.detail.as_str()));
    m.insert("maxNodes".into(), json!(opts.max_nodes));
    m.insert("maxFiles".into(), json!(opts.max_files));
    m.insert("maxFlowSteps".into(), json!(opts.max_flow_steps));
    m.insert("maxOutputTokens".into(), json!(opts.max_output_tokens));
    Value::Object(m)
}

fn summary_skeleton(suggestions: &[String]) -> Value {
    let omitted: Map<String, Value> = SUMMARY_LIST_KEYS.iter().map(|k| (k.to_string(), json!(SIZE_PROBE))).collect();
    json!({
        "type": "summary",
        "matchedNodes": SIZE_PROBE, "returnedNodes": SIZE_PROBE, "returnedEdges": SIZE_PROBE,
        "maxOutputTokens": SIZE_PROBE, "truncated": true, "suggestedNextCommands": suggestions,
        "estimatedOutputTokens": SIZE_PROBE, "status": "degraded", "evidenceStrength": "moderate",
        "coveredTerms": vec!["identifier-component"; 12],
        "returnedFiles": vec!["path/to/a/representative/source-file.ts"; 6],
        "sourceBackedNodes": vec!["method:0123456789abcdef0123456789abcdef"; 24],
        "textFallbackFiles": vec!["path/to/a/representative/source-file.ts"; 6],
        "warnings": vec!["Representative graph health warning."; 3],
        "omittedCounts": omitted,
    })
}

/// Tokens reserved for a summary carrying `suggestions`.
pub fn summary_reserve(suggestions: &[String]) -> usize {
    estimate_tokens(&summary_skeleton(suggestions)) + RESERVE_PAD
}

/// Start a response. The summary reserve is sized from the actual summary shape; a ceiling
/// below mandatory framing is rejected rather than silently raised.
pub fn begin_response(
    command: &str,
    opts: &AgentOptions,
    task: Option<&str>,
    anticipated: &[String],
) -> Result<Response, Value> {
    let requested = opts.max_output_tokens;
    let reserve = summary_reserve(anticipated);
    let probe = AgentOptions {
        max_output_tokens: SIZE_PROBE as usize,
        ..opts.clone()
    };
    let floor = estimate_tokens(&meta_record(command, &probe, task)) + reserve;
    if requested < floor {
        return Err(budget_error(requested, floor));
    }
    let meta = meta_record(command, opts, task);
    let mut ledger = BudgetLedger::new(requested, reserve);
    ledger.frame(&meta);
    Ok(Response {
        ledger,
        meta,
        effective_max: requested,
        records: Vec::new(),
    })
}

/// Fields of the trailing summary record.
#[derive(Debug, Clone, Default)]
pub struct SummaryFields {
    pub matched_nodes: usize,
    pub returned_nodes: usize,
    pub returned_edges: usize,
    pub truncated: bool,
    pub suggested_next_commands: Vec<String>,
    pub status: Option<&'static str>,
    pub evidence_strength: Option<&'static str>,
    pub covered_terms: Vec<String>,
    pub returned_files: Vec<String>,
    pub source_backed_nodes: Vec<String>,
    pub text_fallback_files: Vec<String>,
    pub warnings: Vec<String>,
}

fn dedup(v: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in v {
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

fn finalized(used: usize, base: &Map<String, Value>) -> Value {
    let mut probe = base.clone();
    probe.insert("estimatedOutputTokens".into(), json!(SIZE_PROBE));
    let mut estimate = used + estimate_tokens(&Value::Object(probe));
    for _ in 0..4 {
        let mut rec = base.clone();
        rec.insert("estimatedOutputTokens".into(), json!(estimate));
        let exact = used + estimate_tokens(&Value::Object(rec.clone()));
        if exact == estimate {
            return Value::Object(rec);
        }
        estimate = exact;
    }
    let mut rec = base.clone();
    rec.insert("estimatedOutputTokens".into(), json!(estimate));
    Value::Object(rec)
}

impl Response {
    /// Reserve and keep a data record when it fits; false (and truncation noted) otherwise.
    pub fn push(&mut self, record: Value) -> bool {
        if self.ledger.try_add(&record) {
            self.records.push(record);
            true
        } else {
            false
        }
    }

    /// Keep a record that was already accounted by the ledger.
    pub fn push_accounted(&mut self, record: Value) {
        self.records.push(record);
    }

    /// Build the summary (lists capped, then pruned largest-last-entry-first until the whole
    /// response fits), account it, and return every line of the response in order.
    pub fn finish(mut self, fields: SummaryFields) -> Vec<Value> {
        let lists: [Vec<String>; 6] = [
            dedup(&fields.source_backed_nodes),
            dedup(&fields.covered_terms),
            dedup(&fields.returned_files),
            dedup(&fields.text_fallback_files),
            dedup(&fields.suggested_next_commands),
            dedup(&fields.warnings),
        ];
        let mut omitted: Map<String, Value> = Map::new();
        let mut capped: Vec<Vec<String>> = Vec::new();
        for (i, list) in lists.iter().enumerate() {
            let limit = SUMMARY_LIST_LIMITS[i];
            if list.len() > limit {
                omitted.insert(SUMMARY_LIST_KEYS[i].into(), json!(list.len() - limit));
            }
            capped.push(list.iter().take(limit).cloned().collect());
        }
        let any_omitted = !omitted.is_empty();
        let status = fields
            .status
            .unwrap_or(if fields.returned_nodes > 0 { "ok" } else { "no-match" });
        let strength = fields
            .evidence_strength
            .unwrap_or(if fields.returned_nodes > 0 { "strong" } else { "none" });
        let mut base = Map::new();
        base.insert("type".into(), json!("summary"));
        base.insert("matchedNodes".into(), json!(fields.matched_nodes));
        base.insert("returnedNodes".into(), json!(fields.returned_nodes));
        base.insert("returnedEdges".into(), json!(fields.returned_edges));
        base.insert("maxOutputTokens".into(), json!(self.effective_max));
        base.insert(
            "truncated".into(),
            json!(fields.truncated || any_omitted || self.ledger.dropped_any() || self.ledger.over_budget()),
        );
        base.insert("suggestedNextCommands".into(), json!(capped[4]));
        base.insert("status".into(), json!(status));
        base.insert("evidenceStrength".into(), json!(strength));
        base.insert("coveredTerms".into(), json!(capped[1]));
        base.insert("returnedFiles".into(), json!(capped[2]));
        base.insert("sourceBackedNodes".into(), json!(capped[0]));
        base.insert("textFallbackFiles".into(), json!(capped[3]));
        base.insert("warnings".into(), json!(capped[5]));
        base.insert("omittedCounts".into(), Value::Object(omitted));
        let used = self.ledger.estimated_tokens();
        let mut summary = finalized(used, &base);
        while used + estimate_tokens(&summary) > self.effective_max {
            // Drop the list entry with the largest last element (ties: key order).
            let mut best: Option<(usize, usize)> = None;
            for (i, key) in SUMMARY_LIST_KEYS.iter().enumerate() {
                if let Some(last) = base.get(*key).and_then(|v| v.as_array()).and_then(|a| a.last()) {
                    let size = estimate_tokens(last);
                    if best.is_none_or(|(_, s)| size > s) {
                        best = Some((i, size));
                    }
                }
            }
            let Some((i, _)) = best else { break };
            let key = SUMMARY_LIST_KEYS[i];
            if let Some(arr) = base.get_mut(key).and_then(|v| v.as_array_mut()) {
                arr.pop();
            }
            let omitted = base
                .get_mut("omittedCounts")
                .and_then(|v| v.as_object_mut())
                .expect("omittedCounts");
            let n = omitted.get(key).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
            omitted.insert(key.into(), json!(n));
            base.insert("truncated".into(), json!(true));
            summary = finalized(used, &base);
        }
        self.ledger.frame(&summary);
        let mut out = Vec::with_capacity(self.records.len() + 2);
        out.push(self.meta);
        out.extend(self.records);
        out.push(summary);
        out
    }
}

/// Render records as JSONL.
pub fn to_jsonl(records: &[Value]) -> String {
    let mut s = String::new();
    for r in records {
        s.push_str(&serde_json::to_string(r).unwrap_or_default());
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_reserves_and_drops() {
        let mut l = BudgetLedger::new(20, 5);
        let small = json!({"a": 1});
        assert!(l.try_add(&small));
        let big = json!({"text": "x".repeat(200)});
        assert!(!l.try_add(&big));
        assert!(l.dropped_any());
        assert!(!l.over_budget());
    }

    #[test]
    fn tiny_budget_is_rejected() {
        let opts = AgentOptions {
            max_output_tokens: 10,
            ..Default::default()
        };
        let err = begin_response("graph scope", &opts, Some("x"), &[]).err().unwrap();
        assert_eq!(err["code"], "INVALID_OUTPUT_BUDGET");
    }

    #[test]
    fn summary_reports_estimate_within_budget() {
        let opts = AgentOptions {
            max_output_tokens: 1000,
            ..Default::default()
        };
        let mut r = begin_response("graph get", &opts, None, &[]).unwrap();
        assert!(r.push(json!({"type": "fact", "id": "x"})));
        let out = r.finish(SummaryFields {
            returned_nodes: 1,
            ..Default::default()
        });
        let total: usize = out.iter().map(estimate_tokens).sum();
        let summary = out.last().unwrap();
        assert_eq!(summary["estimatedOutputTokens"].as_u64().unwrap() as usize, total);
        assert!(total <= 1000);
    }
}
