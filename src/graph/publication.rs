//! Row-level publication delta.
//!
//! A full publication (rebuild) drops every derived table of the live graph and copies the
//! candidate's rows into it, then rebuilds the node FTS index. An incremental refresh changes
//! a handful of files, so it publishes a delta instead: inside one `BEGIN IMMEDIATE`
//! transaction against the attached candidate, every derived table is diffed row by row and
//! only the rows that differ are deleted, updated or inserted. Unchanged rows (and their FTS
//! entries) are never touched. The result is exactly the candidate's rows (bookkeeping
//! timestamps aside: an unchanged node keeps the `updated_at` of the publication that last
//! changed it, an unchanged file its `indexed_at`).
//!
//! Ordering keeps the live foreign keys satisfied without a cascade ever removing a row the
//! candidate still has: stale edges / references / bindings / fingerprints go first, then
//! nodes the candidate no longer has, then changed nodes are updated in place (never deleted
//! and re-inserted) and new rows are inserted.

use rusqlite::{Connection, Result};
use serde::{Deserialize, Serialize};

/// Rows a delta publication removed and wrote (insert or update), per table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeltaStats {
    pub deleted: std::collections::BTreeMap<String, usize>,
    pub written: std::collections::BTreeMap<String, usize>,
}

impl DeltaStats {
    pub fn rows_deleted(&self) -> usize {
        self.deleted.values().sum()
    }
    pub fn rows_written(&self) -> usize {
        self.written.values().sum()
    }
    fn del(&mut self, t: &str, n: usize) {
        *self.deleted.entry(t.to_string()).or_default() += n;
    }
    fn put(&mut self, t: &str, n: usize) {
        *self.written.entry(t.to_string()).or_default() += n;
    }
}

const NODE_COLS: [&str; 24] = [
    "kind", "name", "qualified_name", "container_id", "identity_key", "file_path", "language",
    "start_line", "end_line", "start_column", "end_column", "docstring", "signature", "visibility",
    "is_exported", "is_async", "is_static", "is_abstract", "decorators", "type_parameters",
    "return_type", "body_hash", "updated_at", "id",
];
const EDGE_COLS: [&str; 10] = [
    "source", "target", "kind", "metadata", "line", "col", "provenance", "confidence",
    "resolution_method", "evidence",
];
const REF_COLS: [&str; 17] = [
    "ref_key", "from_node_id", "reference_name", "reference_kind", "line", "col", "candidates",
    "file_path", "language", "receiver", "qualifier", "import_source", "metadata", "status",
    "target_id", "confidence", "resolver",
];
const BINDING_COLS: [&str; 9] = [
    "binding_key", "file_path", "local_name", "imported_name", "module_specifier",
    "resolved_file_path", "target_id", "is_type_only", "metadata",
];
const FILE_COLS: [&str; 13] = [
    "path", "content_hash", "language", "size", "modified_at", "indexed_at", "node_count", "errors",
    "parse_status", "diagnostic_count", "missing_count", "error_coverage", "extractor_version",
];
const CACHE_COLS: [&str; 4] = ["path", "content_hash", "extractor_version", "payload"];
const CHUNK_COLS: [&str; 7] = [
    "file_path", "content_hash", "start_line", "end_line", "path_terms", "identifier_terms",
    "comment_terms",
];

/// Columns declared NOT NULL (compared with `=` so the probes use their indexes).
const NOT_NULL: [&str; 9] = ["id", "source", "target", "kind", "node_id", "band", "bucket", "path", "file_path"];

/// `a.c = b.c AND ...` over `cols` (null-safe `IS` for nullable columns), skipping `except`.
fn same(cols: &[&str], a: &str, b: &str, except: &[&str]) -> String {
    cols.iter()
        .filter(|c| !except.contains(c))
        .map(|c| {
            if NOT_NULL.contains(c) {
                format!("{a}.{c} = {b}.{c}")
            } else {
                format!("{a}.{c} IS {b}.{c}")
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn list(cols: &[&str], prefix: &str) -> String {
    cols.iter().map(|c| format!("{prefix}{c}")).collect::<Vec<_>>().join(", ")
}

fn cmp_cols<'a>(cols: &[&'a str], ignore: &[&str]) -> Vec<&'a str> {
    cols.iter().copied().filter(|c| !ignore.contains(c)).collect()
}

/// Delete live rows of `table` whose `key` the candidate no longer has, then upsert candidate
/// rows that are new or whose columns (minus the bookkeeping `ignore` columns, which are not
/// compared but are written when a row changes) differ. Changed rows are updated in place.
/// Differences are found with set operations (`EXCEPT`), one sorted pass per side.
fn keyed_delta(
    live: &Connection,
    stats: &mut DeltaStats,
    table: &str,
    cols: &[&str],
    key: &str,
    ignore: &[&str],
) -> Result<()> {
    let n = live.execute(
        &format!("DELETE FROM main.{table} WHERE {key} NOT IN (SELECT {key} FROM cand.{table})"),
        [],
    )?;
    stats.del(table, n);
    let updates = cols
        .iter()
        .filter(|c| **c != key)
        .map(|c| format!("{c} = excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let cmp = list(&cmp_cols(cols, ignore), "");
    let n = live.execute(
        &format!(
            "INSERT INTO main.{table} ({cols}) SELECT {cols} FROM cand.{table} \
             WHERE {key} IN (SELECT {key} FROM (SELECT {cmp} FROM cand.{table} EXCEPT SELECT {cmp} FROM main.{table})) \
             ON CONFLICT({key}) DO UPDATE SET {updates}",
            cols = list(cols, ""),
        ),
        [],
    )?;
    stats.put(table, n);
    Ok(())
}

/// Delete live rows of `table` keyed by `key` that differ from (or are absent in) the
/// candidate. The candidate's versions are inserted later by [`keyed_insert_missing`].
fn keyed_delete_stale(live: &Connection, stats: &mut DeltaStats, table: &str, cols: &[&str], key: &str) -> Result<()> {
    let cmp = list(cols, "");
    let n = live.execute(
        &format!(
            "DELETE FROM main.{table} WHERE {key} IN \
             (SELECT {key} FROM (SELECT {cmp} FROM main.{table} EXCEPT SELECT {cmp} FROM cand.{table}))"
        ),
        [],
    )?;
    stats.del(table, n);
    Ok(())
}

/// Insert candidate rows of `table` whose `key` the live table lacks.
fn keyed_insert_missing(live: &Connection, stats: &mut DeltaStats, table: &str, cols: &[&str], key: &str) -> Result<()> {
    let n = live.execute(
        &format!(
            "INSERT INTO main.{table} ({cols}) SELECT {cols} FROM cand.{table} \
             WHERE {key} NOT IN (SELECT {key} FROM main.{table})",
            cols = list(cols, ""),
        ),
        [],
    )?;
    stats.put(table, n);
    Ok(())
}

/// Delete live rows of an unkeyed `table` that have no identical candidate row (`id_col`
/// identifies them).
fn multiset_delete(live: &Connection, stats: &mut DeltaStats, table: &str, cols: &[&str], id_col: &str) -> Result<()> {
    let c = list(cols, "");
    live.execute_batch(&format!(
        "DROP TABLE IF EXISTS temp._delta_stale;
         CREATE TEMP TABLE _delta_stale AS SELECT {c} FROM main.{table} EXCEPT SELECT {c} FROM cand.{table};"
    ))?;
    let n = live.execute(
        &format!(
            "DELETE FROM main.{table} WHERE {id_col} IN (SELECT m.{id_col} FROM temp._delta_stale s \
             JOIN main.{table} m ON {same})",
            same = same(cols, "m", "s", &[]),
        ),
        [],
    )?;
    live.execute_batch("DROP TABLE IF EXISTS temp._delta_stale;")?;
    stats.del(table, n);
    Ok(())
}

/// Insert candidate rows of an unkeyed `table` the live table lacks.
fn multiset_insert(live: &Connection, stats: &mut DeltaStats, table: &str, cols: &[&str]) -> Result<()> {
    let c = list(cols, "");
    let n = live.execute(
        &format!("INSERT INTO main.{table} ({c}) SELECT {c} FROM cand.{table} EXCEPT SELECT {c} FROM main.{table}"),
        [],
    )?;
    stats.put(table, n);
    Ok(())
}

/// Apply the candidate attached as `cand` to the live graph as a row delta. Must run inside a
/// write transaction on `live`; the caller commits.
pub(crate) fn apply_delta(live: &Connection) -> Result<DeltaStats> {
    let mut stats = DeltaStats::default();
    // 1. Rows hanging off nodes that changed or disappeared.
    multiset_delete(live, &mut stats, "edges", &EDGE_COLS, "id")?;
    keyed_delete_stale(live, &mut stats, "unresolved_refs", &REF_COLS, "ref_key")?;
    keyed_delete_stale(live, &mut stats, "import_bindings", &BINDING_COLS, "binding_key")?;
    // A node's LSH buckets are a function of its MinHash sketch: the nodes whose sketch
    // changed, appeared or vanished are exactly those whose bucket rows are replaced. (Buckets
    // are shared by many nodes, so a row-wise bucket diff would be quadratic.)
    live.execute_batch(
        "DROP TABLE IF EXISTS temp._delta_sketch_nodes;
         CREATE TEMP TABLE _delta_sketch_nodes AS
           SELECT m.node_id FROM main.node_minhash m WHERE NOT EXISTS (SELECT 1 FROM cand.node_minhash c
             WHERE c.node_id = m.node_id AND c.minhash IS m.minhash AND c.token_count IS m.token_count)
           UNION
           SELECT c.node_id FROM cand.node_minhash c WHERE NOT EXISTS (SELECT 1 FROM main.node_minhash m
             WHERE m.node_id = c.node_id AND m.minhash IS c.minhash AND m.token_count IS c.token_count);",
    )?;
    let n = live.execute(
        "DELETE FROM main.node_minhash WHERE node_id IN (SELECT node_id FROM temp._delta_sketch_nodes)",
        [],
    )?;
    stats.del("node_minhash", n);
    let n = live.execute(
        "DELETE FROM main.node_lsh WHERE node_id IN (SELECT node_id FROM temp._delta_sketch_nodes)",
        [],
    )?;
    stats.del("node_lsh", n);

    // 2. Nodes: drop the vanished, update the changed in place, insert the new.
    keyed_delta(live, &mut stats, "nodes", &NODE_COLS, "id", &["updated_at"])?;

    // 3. New rows hanging off nodes.
    multiset_insert(live, &mut stats, "edges", &EDGE_COLS)?;
    keyed_insert_missing(live, &mut stats, "unresolved_refs", &REF_COLS, "ref_key")?;
    keyed_insert_missing(live, &mut stats, "import_bindings", &BINDING_COLS, "binding_key")?;
    let n = live.execute(
        "INSERT INTO main.node_minhash (node_id, minhash, token_count) SELECT c.node_id, c.minhash, c.token_count \
         FROM cand.node_minhash c WHERE c.node_id IN (SELECT node_id FROM temp._delta_sketch_nodes)",
        [],
    )?;
    stats.put("node_minhash", n);
    let n = live.execute(
        "INSERT INTO main.node_lsh (band, bucket, node_id) SELECT c.band, c.bucket, c.node_id \
         FROM cand.node_lsh c WHERE c.node_id IN (SELECT node_id FROM temp._delta_sketch_nodes)",
        [],
    )?;
    stats.put("node_lsh", n);
    live.execute_batch("DROP TABLE IF EXISTS temp._delta_sketch_nodes;")?;

    // 4. Per-file tables.
    keyed_delta(live, &mut stats, "files", &FILE_COLS, "path", &["indexed_at"])?;
    // The payload is a function of (content hash, extractor version): compared through them.
    keyed_delta(live, &mut stats, "file_extraction_cache", &CACHE_COLS, "path", &["payload"])?;

    // 5. Source chunks are a function of (file, content hash): replace the files whose pair
    // changed.
    live.execute_batch(
        "DROP TABLE IF EXISTS temp._delta_chunk_files;
         CREATE TEMP TABLE _delta_chunk_files AS
           SELECT file_path FROM (
             SELECT * FROM (SELECT DISTINCT file_path, content_hash FROM main.code_chunks
                            EXCEPT SELECT DISTINCT file_path, content_hash FROM cand.code_chunks)
             UNION
             SELECT * FROM (SELECT DISTINCT file_path, content_hash FROM cand.code_chunks
                            EXCEPT SELECT DISTINCT file_path, content_hash FROM main.code_chunks));",
    )?;
    let changed_chunk_files: i64 =
        live.query_row("SELECT COUNT(*) FROM temp._delta_chunk_files", [], |r| r.get(0))?;
    if changed_chunk_files > 0 {
        let n = live.execute(
            "DELETE FROM main.code_chunks WHERE file_path IN (SELECT file_path FROM temp._delta_chunk_files)",
            [],
        )?;
        stats.del("code_chunks", n);
        let n = live.execute(
            &format!(
                "INSERT INTO main.code_chunks ({cols}) SELECT {cols} FROM cand.code_chunks \
                 WHERE file_path IN (SELECT file_path FROM temp._delta_chunk_files)",
                cols = list(&CHUNK_COLS, ""),
            ),
            [],
        )?;
        stats.put("code_chunks", n);
    }
    live.execute_batch("DROP TABLE IF EXISTS temp._delta_chunk_files;")?;

    // 6. Build metadata.
    live.execute_batch(
        "INSERT OR REPLACE INTO main.project_metadata (key, value, updated_at) \
         SELECT key, value, updated_at FROM cand.project_metadata;",
    )?;
    Ok(stats)
}
