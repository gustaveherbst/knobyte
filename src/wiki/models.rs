use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EntityRelation {
    #[serde(rename = "type")]
    pub rel_type: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EntityGrounding {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Frontmatter {
    pub id: Option<String>,
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub entity_type: Option<String>,
    pub summary: Option<String>,
    pub status: Option<String>,
    pub revision: Option<i64>,
    #[serde(default)]
    pub relations: Vec<EntityRelation>,
    #[serde(default)]
    pub grounds_to: Vec<serde_json::Value>,
    #[serde(default)]
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiEntity {
    pub entity_key: String,
    pub id: String,
    pub file: String,
    pub entity_type: String,
    pub title: String,
    pub summary: Option<String>,
    pub body: String,
    pub status: String,
    pub revision: i64,
    pub relations: Vec<EntityRelation>,
    pub grounds_to: Vec<String>,
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiDiagnostic {
    pub code: String,
    pub message: String,
    pub file: String,
    pub line: Option<usize>,
}
