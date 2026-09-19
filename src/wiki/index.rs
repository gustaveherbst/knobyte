use std::fs;
use std::path::Path;
use rusqlite::{params, Connection, Result};
use walkdir::WalkDir;

use crate::wiki::models::{EntityRelation, WikiDiagnostic, WikiEntity};
use crate::wiki::parser::parse_markdown_entity;

pub struct WikiIndex {
    conn: Connection,
}

impl WikiIndex {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let conn = Connection::open(db_path)?;
        Self::initialize_schema(&conn)?;
        Ok(Self { conn })
    }

    fn initialize_schema(conn: &Connection) -> Result<()> {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "foreign_keys", "ON");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS wiki_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS wiki_files (
                path TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                parse_status TEXT NOT NULL,
                entity_count INTEGER NOT NULL,
                text_length INTEGER NOT NULL,
                indexed_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS wiki_entities (
                entity_key TEXT PRIMARY KEY,
                id TEXT NOT NULL,
                shadowed INTEGER NOT NULL DEFAULT 0,
                file TEXT NOT NULL,
                type TEXT NOT NULL,
                title TEXT NOT NULL,
                summary TEXT,
                body TEXT NOT NULL,
                status TEXT NOT NULL,
                revision INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS wiki_relations (
                source_key TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                type TEXT NOT NULL,
                target_id TEXT NOT NULL,
                target_resolved INTEGER NOT NULL DEFAULT 0,
                note TEXT,
                PRIMARY KEY (source_key, ordinal)
            );

            CREATE TABLE IF NOT EXISTS wiki_groundings (
                entity_key TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                node_id TEXT NOT NULL,
                PRIMARY KEY (entity_key, ordinal)
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS wiki_fts USING fts5(
                entity_key,
                id,
                title,
                summary,
                body,
                type
            );

            CREATE TRIGGER IF NOT EXISTS wiki_entities_ai AFTER INSERT ON wiki_entities BEGIN
                INSERT INTO wiki_fts(entity_key, id, title, summary, body, type)
                VALUES (NEW.entity_key, NEW.id, NEW.title, NEW.summary, NEW.body, NEW.type);
            END;

            CREATE TRIGGER IF NOT EXISTS wiki_entities_ad AFTER DELETE ON wiki_entities BEGIN
                DELETE FROM wiki_fts WHERE entity_key = OLD.entity_key;
            END;

            CREATE INDEX IF NOT EXISTS idx_wiki_entities_id ON wiki_entities(id);
            CREATE INDEX IF NOT EXISTS idx_wiki_entities_file ON wiki_entities(file);
            CREATE INDEX IF NOT EXISTS idx_wiki_relations_target ON wiki_relations(target_id);
            CREATE INDEX IF NOT EXISTS idx_wiki_groundings_node ON wiki_groundings(node_id);
            "#,
        )?;
        Ok(())
    }

    pub fn rebuild(&mut self, scaffold_root: &Path) -> Result<usize> {
        let tx = self.conn.transaction()?;

        tx.execute_batch(
            r#"
            DELETE FROM wiki_entities;
            DELETE FROM wiki_relations;
            DELETE FROM wiki_groundings;
            DELETE FROM wiki_files;
            "#,
        )?;

        let mut indexed_entities = 0;

        for entry in WalkDir::new(scaffold_root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext != "md" {
                continue;
            }

            let rel_path = match path.strip_prefix(scaffold_root) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => path.to_string_lossy().to_string(),
            };

            // Skip local/ directory and dotfiles
            if rel_path.starts_with("local/") || rel_path.starts_with('.') {
                continue;
            }

            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if let Some(entity) = parse_markdown_entity(&rel_path, &content) {
                let now_str = chrono::Utc::now().to_rfc3339();

                tx.execute(
                    r#"
                    INSERT OR REPLACE INTO wiki_files (
                        path, content_hash, parse_status, entity_count, text_length, indexed_at
                    ) VALUES (?1, '', 'ok', 1, ?2, ?3)
                    "#,
                    params![rel_path, content.len() as i64, now_str],
                )?;

                tx.execute(
                    r#"
                    INSERT OR REPLACE INTO wiki_entities (
                        entity_key, id, shadowed, file, type, title, summary, body, status, revision
                    ) VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                    "#,
                    params![
                        entity.entity_key,
                        entity.id,
                        entity.file,
                        entity.entity_type,
                        entity.title,
                        entity.summary,
                        entity.body,
                        entity.status,
                        entity.revision,
                    ],
                )?;

                for (idx, rel) in entity.relations.iter().enumerate() {
                    tx.execute(
                        r#"
                        INSERT OR REPLACE INTO wiki_relations (
                            source_key, ordinal, type, target_id, note
                        ) VALUES (?1, ?2, ?3, ?4, ?5)
                        "#,
                        params![entity.entity_key, idx as i64, rel.rel_type, rel.target_id, rel.note],
                    )?;
                }

                for (idx, node_id) in entity.grounds_to.iter().enumerate() {
                    tx.execute(
                        r#"
                        INSERT OR REPLACE INTO wiki_groundings (
                            entity_key, ordinal, node_id
                        ) VALUES (?1, ?2, ?3)
                        "#,
                        params![entity.entity_key, idx as i64, node_id],
                    )?;
                }

                indexed_entities += 1;
            }
        }

        // Post-index: resolve target_resolved
        tx.execute_batch(
            r#"
            UPDATE wiki_relations
            SET target_resolved = 1
            WHERE target_id IN (SELECT id FROM wiki_entities);
            "#,
        )?;

        let now_str = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "INSERT OR REPLACE INTO wiki_meta (key, value) VALUES ('last_rebuild', ?1)",
            params![now_str],
        )?;

        tx.commit()?;
        Ok(indexed_entities)
    }

    pub fn query(&self, text: &str) -> Result<Vec<WikiEntity>> {
        let tokens: Vec<String> = text
            .split_whitespace()
            .map(|w| format!("\"{}\"*", w.replace('"', "")))
            .collect();

        if tokens.is_empty() {
            return self.list();
        }

        let fts_query = tokens.join(" OR ");

        let mut stmt = self.conn.prepare(
            r#"
            SELECT e.entity_key, e.id, e.file, e.type, e.title, e.summary, e.body, e.status, e.revision
            FROM wiki_entities e
            JOIN wiki_fts f ON e.entity_key = f.entity_key
            WHERE wiki_fts MATCH ?1
            ORDER BY rank
            LIMIT 50
            "#,
        )?;

        let rows = stmt.query_map(params![fts_query], |r| {
            Ok(WikiEntity {
                entity_key: r.get(0)?,
                id: r.get(1)?,
                file: r.get(2)?,
                entity_type: r.get(3)?,
                title: r.get(4)?,
                summary: r.get(5)?,
                body: r.get(6)?,
                status: r.get(7)?,
                revision: r.get(8)?,
                relations: Vec::new(),
                grounds_to: Vec::new(),
                topics: Vec::new(),
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            let mut entity = r?;
            self.load_relations_and_groundings(&mut entity)?;
            results.push(entity);
        }

        Ok(results)
    }

    pub fn show(&self, id: &str) -> Result<Option<WikiEntity>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT entity_key, id, file, type, title, summary, body, status, revision
            FROM wiki_entities
            WHERE id = ?1 OR entity_key = ?1
            LIMIT 1
            "#,
        )?;

        let entity_res = stmt.query_row(params![id], |r| {
            Ok(WikiEntity {
                entity_key: r.get(0)?,
                id: r.get(1)?,
                file: r.get(2)?,
                entity_type: r.get(3)?,
                title: r.get(4)?,
                summary: r.get(5)?,
                body: r.get(6)?,
                status: r.get(7)?,
                revision: r.get(8)?,
                relations: Vec::new(),
                grounds_to: Vec::new(),
                topics: Vec::new(),
            })
        });

        match entity_res {
            Ok(mut entity) => {
                self.load_relations_and_groundings(&mut entity)?;
                Ok(Some(entity))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn list(&self) -> Result<Vec<WikiEntity>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT entity_key, id, file, type, title, summary, body, status, revision
            FROM wiki_entities
            ORDER BY type, title
            "#,
        )?;

        let rows = stmt.query_map([], |r| {
            Ok(WikiEntity {
                entity_key: r.get(0)?,
                id: r.get(1)?,
                file: r.get(2)?,
                entity_type: r.get(3)?,
                title: r.get(4)?,
                summary: r.get(5)?,
                body: r.get(6)?,
                status: r.get(7)?,
                revision: r.get(8)?,
                relations: Vec::new(),
                grounds_to: Vec::new(),
                topics: Vec::new(),
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn related(&self, id: &str) -> Result<Vec<WikiEntity>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT DISTINCT e.entity_key, e.id, e.file, e.type, e.title, e.summary, e.body, e.status, e.revision
            FROM wiki_entities e
            JOIN wiki_relations r ON (e.id = r.target_id)
            JOIN wiki_entities s ON (r.source_key = s.entity_key)
            WHERE s.id = ?1
            UNION
            SELECT DISTINCT e.entity_key, e.id, e.file, e.type, e.title, e.summary, e.body, e.status, e.revision
            FROM wiki_entities e
            JOIN wiki_relations r ON (e.entity_key = r.source_key)
            WHERE r.target_id = ?1
            "#,
        )?;

        let rows = stmt.query_map(params![id], |r| {
            Ok(WikiEntity {
                entity_key: r.get(0)?,
                id: r.get(1)?,
                file: r.get(2)?,
                entity_type: r.get(3)?,
                title: r.get(4)?,
                summary: r.get(5)?,
                body: r.get(6)?,
                status: r.get(7)?,
                revision: r.get(8)?,
                relations: Vec::new(),
                grounds_to: Vec::new(),
                topics: Vec::new(),
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn backlinks(&self, id: &str) -> Result<Vec<WikiEntity>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT DISTINCT e.entity_key, e.id, e.file, e.type, e.title, e.summary, e.body, e.status, e.revision
            FROM wiki_entities e
            JOIN wiki_relations r ON (e.entity_key = r.source_key)
            WHERE r.target_id = ?1
            "#,
        )?;

        let rows = stmt.query_map(params![id], |r| {
            Ok(WikiEntity {
                entity_key: r.get(0)?,
                id: r.get(1)?,
                file: r.get(2)?,
                entity_type: r.get(3)?,
                title: r.get(4)?,
                summary: r.get(5)?,
                body: r.get(6)?,
                status: r.get(7)?,
                revision: r.get(8)?,
                relations: Vec::new(),
                grounds_to: Vec::new(),
                topics: Vec::new(),
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn for_code(&self, node_id: &str) -> Result<Vec<WikiEntity>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT DISTINCT e.entity_key, e.id, e.file, e.type, e.title, e.summary, e.body, e.status, e.revision
            FROM wiki_entities e
            JOIN wiki_groundings g ON e.entity_key = g.entity_key
            WHERE g.node_id = ?1
            "#,
        )?;

        let rows = stmt.query_map(params![node_id], |r| {
            Ok(WikiEntity {
                entity_key: r.get(0)?,
                id: r.get(1)?,
                file: r.get(2)?,
                entity_type: r.get(3)?,
                title: r.get(4)?,
                summary: r.get(5)?,
                body: r.get(6)?,
                status: r.get(7)?,
                revision: r.get(8)?,
                relations: Vec::new(),
                grounds_to: Vec::new(),
                topics: Vec::new(),
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn validate(&self) -> Result<Vec<WikiDiagnostic>> {
        let mut diagnostics = Vec::new();

        // Check dangling relations
        let mut stmt = self.conn.prepare(
            r#"
            SELECT e.file, r.type, r.target_id
            FROM wiki_relations r
            JOIN wiki_entities e ON r.source_key = e.entity_key
            WHERE r.target_resolved = 0
            "#,
        )?;

        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;

        for r in rows {
            let (file, rel_type, target_id) = r?;
            diagnostics.push(WikiDiagnostic {
                code: "DANGLING_RELATION_TARGET".to_string(),
                message: format!("Relation '{}' points to nonexistent entity '{}'", rel_type, target_id),
                file,
                line: None,
            });
        }

        Ok(diagnostics)
    }

    fn load_relations_and_groundings(&self, entity: &mut WikiEntity) -> Result<()> {
        let mut rel_stmt = self.conn.prepare(
            "SELECT type, target_id, note FROM wiki_relations WHERE source_key = ?1 ORDER BY ordinal",
        )?;
        let rel_rows = rel_stmt.query_map(params![entity.entity_key], |r| {
            Ok(EntityRelation {
                rel_type: r.get(0)?,
                target_id: r.get(1)?,
                note: r.get(2)?,
            })
        })?;
        for r in rel_rows {
            entity.relations.push(r?);
        }

        let mut gr_stmt = self.conn.prepare(
            "SELECT node_id FROM wiki_groundings WHERE entity_key = ?1 ORDER BY ordinal",
        )?;
        let gr_rows = gr_stmt.query_map(params![entity.entity_key], |r| r.get(0))?;
        for r in gr_rows {
            entity.grounds_to.push(r?);
        }

        Ok(())
    }
}
