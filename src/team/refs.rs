//! Typed references shared by Inbox proposals and Relays.

use serde::{Deserialize, Serialize};

/// Evidence supporting a proposal or handoff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum EvidenceRef {
    Entity {
        id: String,
        #[serde(default, rename = "entityKind", skip_serializing_if = "Option::is_none")]
        entity_kind: Option<String>,
    },
    Code {
        #[serde(rename = "symbolId")]
        symbol_id: String,
    },
    Commit {
        hash: String,
    },
    File {
        path: String,
    },
    External {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Manual {
        note: String,
    },
}

impl EvidenceRef {
    /// Parse `entity:<id>`, `code:<symbol>`, `commit:<hash>`, `file:<path>`,
    /// `external:<uri>` (or a bare `http(s)://` URL), `manual:<note>`. Any other
    /// text is kept as a manual note.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("Evidence must not be empty".to_string());
        }
        if spec.len() > 2048 {
            return Err("Evidence entries must be at most 2048 bytes".to_string());
        }
        if spec.starts_with("http://") || spec.starts_with("https://") {
            return Ok(EvidenceRef::External { uri: spec.to_string(), label: None });
        }
        let parsed = spec.split_once(':').map(|(k, v)| (k, v.trim()));
        Ok(match parsed {
            Some(("entity", v)) if !v.is_empty() => EvidenceRef::Entity { id: v.to_string(), entity_kind: None },
            Some(("code", v)) if !v.is_empty() => EvidenceRef::Code { symbol_id: v.to_string() },
            Some(("commit", v)) if is_commit(v) => EvidenceRef::Commit { hash: v.to_string() },
            Some(("commit", v)) => return Err(format!("Invalid commit hash '{}'", v)),
            Some(("file", v)) if !v.is_empty() => {
                if v.starts_with('/') || v.split('/').any(|p| p == "..") {
                    return Err(format!("Evidence file '{}' must be repository-relative", v));
                }
                EvidenceRef::File { path: v.to_string() }
            }
            Some(("external", v)) if !v.is_empty() => EvidenceRef::External { uri: v.to_string(), label: None },
            Some(("manual", v)) if !v.is_empty() => EvidenceRef::Manual { note: v.to_string() },
            _ => EvidenceRef::Manual { note: spec.to_string() },
        })
    }

    /// Human rendering.
    pub fn display(&self) -> String {
        match self {
            EvidenceRef::Entity { id, .. } => format!("entity {}", id),
            EvidenceRef::Code { symbol_id } => format!("code {}", symbol_id),
            EvidenceRef::Commit { hash } => format!("commit {}", hash),
            EvidenceRef::File { path } => format!("file {}", path),
            EvidenceRef::External { uri, label } => match label {
                Some(l) => format!("{} <{}>", l, uri),
                None => uri.clone(),
            },
            EvidenceRef::Manual { note } => note.clone(),
        }
    }
}

fn is_commit(v: &str) -> bool {
    (4..=64).contains(&v.len()) && v.chars().all(|c| c.is_ascii_hexdigit())
}

/// Code reference: a graph symbol id or a repository-relative file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CodeRef {
    Symbol {
        #[serde(rename = "symbolId")]
        symbol_id: String,
    },
    File {
        path: String,
    },
}

impl CodeRef {
    /// `file:<path>` / `symbol:<id>`; a value containing `/` or a file extension
    /// without a prefix is treated as a file.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() || spec.len() > 1024 {
            return Err("Code references must be 1-1024 bytes".to_string());
        }
        if let Some(p) = spec.strip_prefix("file:") {
            return Ok(CodeRef::File { path: p.trim().to_string() });
        }
        if let Some(s) = spec.strip_prefix("symbol:") {
            return Ok(CodeRef::Symbol { symbol_id: s.trim().to_string() });
        }
        Ok(CodeRef::Symbol { symbol_id: spec.to_string() })
    }

    pub fn display(&self) -> String {
        match self {
            CodeRef::Symbol { symbol_id } => symbol_id.clone(),
            CodeRef::File { path } => path.clone(),
        }
    }
}

/// Parse a list of evidence specs.
pub fn parse_evidence(specs: &[String]) -> Result<Vec<EvidenceRef>, String> {
    let mut out: Vec<EvidenceRef> = Vec::new();
    for s in specs {
        let e = EvidenceRef::parse(s)?;
        if !out.contains(&e) {
            out.push(e);
        }
    }
    if out.len() > 64 {
        return Err("At most 64 evidence references are allowed".to_string());
    }
    Ok(out)
}
