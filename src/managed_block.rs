//! Merging a Knobyte-owned block into a file the user already wrote.
//!
//! Two callers share this: the skills installer maintains the agent-skills policy block in the
//! root `CLAUDE.md` / `AGENTS.md`, and setup maintains a scaffold pointer block in the anchors
//! that have no skills (`.cursorrules`, `.windsurfrules`, `.github/copilot-instructions.md`).
//! Both write into hand-written files whose bytes outside the block must survive byte for byte,
//! so marker parsing, line-ending detection and the append/replace decision live here once.

use sha2::{Digest, Sha256};

/// What an edit would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockAction {
    /// The file does not exist; it is created with just the block.
    Create,
    /// The file is a byte-exact copy of a known legacy Knobyte output; replaced wholesale.
    Migrate,
    /// The block is appended (no markers yet) or the existing block is replaced.
    Update,
    /// Nothing to do.
    Noop,
    /// The file cannot be edited safely and is left untouched.
    Conflict,
}

/// Why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    Absent,
    Legacy,
    Append,
    Replace,
    Exact,
    TooLarge,
    MalformedMarkers,
    InvalidEncoding,
}

impl BlockReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockReason::Absent => "absent",
            BlockReason::Legacy => "legacy",
            BlockReason::Append => "append",
            BlockReason::Replace => "replace",
            BlockReason::Exact => "exact",
            BlockReason::TooLarge => "managed-block-too-large",
            BlockReason::MalformedMarkers => "malformed-markers",
            BlockReason::InvalidEncoding => "invalid-encoding",
        }
    }
}

/// Bounded preview of the change: never contains user bytes outside the markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockChange {
    /// `create`, `append`, `replace` or `known-legacy-migration`.
    pub scope: &'static str,
    /// The exact previous marker-delimited block, if any.
    pub before: Option<String>,
    /// The exact block that will be installed.
    pub after: String,
}

#[derive(Debug, Clone)]
pub struct BlockEdit {
    pub action: BlockAction,
    pub reason: BlockReason,
    /// Full desired file content (for create/migrate/update).
    pub desired: Option<Vec<u8>>,
    pub change: Option<BlockChange>,
}

/// Describes one managed block.
pub struct BlockSpec<'a> {
    pub start: &'a str,
    pub end: &'a str,
    /// Render the full block (markers included) with the given line ending.
    pub render: &'a dyn Fn(&str) -> String,
    /// Refuse to edit an existing block larger than this many bytes.
    pub max_block_bytes: usize,
    /// SHA-256 (hex) of files a previous Knobyte version wrote verbatim.
    pub legacy_hashes: &'a [&'a str],
    /// A file that already points at the scaffold by other means needs no block.
    pub is_already_pointing: Option<&'a dyn Fn(&str) -> bool>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `\r\n` when the content uses it anywhere, otherwise `\n`.
pub fn detect_eol(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn all_indexes(content: &str, needle: &str) -> Vec<usize> {
    content.match_indices(needle).map(|(i, _)| i).collect()
}

fn standalone(content: &str, index: usize, marker: &str) -> bool {
    let bytes = content.as_bytes();
    let begins = index == 0 || bytes[index - 1] == b'\n';
    let after = index + marker.len();
    let ends = after == bytes.len()
        || bytes[after] == b'\n'
        || (bytes[after] == b'\r' && bytes.get(after + 1) == Some(&b'\n'));
    begins && ends
}

/// Compute an edit without writing. Bytes outside a valid managed block are copied unchanged.
pub fn plan_block_edit(spec: &BlockSpec<'_>, current: Option<&[u8]>) -> BlockEdit {
    let Some(bytes) = current else {
        let after = (spec.render)("\n");
        return BlockEdit {
            action: BlockAction::Create,
            reason: BlockReason::Absent,
            desired: Some(format!("{}\n", after).into_bytes()),
            change: Some(BlockChange { scope: "create", before: None, after }),
        };
    };
    let Ok(content) = std::str::from_utf8(bytes) else {
        return conflict(BlockReason::InvalidEncoding);
    };

    let starts = all_indexes(content, spec.start);
    let ends = all_indexes(content, spec.end);
    let eol = detect_eol(content);

    if starts.is_empty() && ends.is_empty() {
        if spec.legacy_hashes.contains(&sha256_hex(bytes).as_str()) {
            let after = (spec.render)(eol);
            return BlockEdit {
                action: BlockAction::Migrate,
                reason: BlockReason::Legacy,
                desired: Some(format!("{}{}", after, eol).into_bytes()),
                change: Some(BlockChange { scope: "known-legacy-migration", before: None, after }),
            };
        }
        if let Some(pointing) = spec.is_already_pointing {
            if pointing(content) {
                return BlockEdit { action: BlockAction::Noop, reason: BlockReason::Exact, desired: None, change: None };
            }
        }
        let after = (spec.render)(eol);
        let separator = if content.is_empty() {
            String::new()
        } else if content.ends_with('\n') {
            eol.to_string()
        } else {
            format!("{}{}", eol, eol)
        };
        return BlockEdit {
            action: BlockAction::Update,
            reason: BlockReason::Append,
            desired: Some(format!("{}{}{}{}", content, separator, after, eol).into_bytes()),
            change: Some(BlockChange { scope: "append", before: None, after }),
        };
    }

    if starts.len() != 1
        || ends.len() != 1
        || starts[0] >= ends[0]
        || !standalone(content, starts[0], spec.start)
        || !standalone(content, ends[0], spec.end)
    {
        return conflict(BlockReason::MalformedMarkers);
    }

    let block_end = ends[0] + spec.end.len();
    let before = &content[starts[0]..block_end];
    if before.len() > spec.max_block_bytes {
        return conflict(BlockReason::TooLarge);
    }
    let after = (spec.render)(eol);
    let desired = format!("{}{}{}", &content[..starts[0]], after, &content[block_end..]);
    if desired == content {
        return BlockEdit { action: BlockAction::Noop, reason: BlockReason::Exact, desired: None, change: None };
    }
    BlockEdit {
        action: BlockAction::Update,
        reason: BlockReason::Replace,
        desired: Some(desired.into_bytes()),
        change: Some(BlockChange { scope: "replace", before: Some(before.to_string()), after }),
    }
}

fn conflict(reason: BlockReason) -> BlockEdit {
    BlockEdit { action: BlockAction::Conflict, reason, desired: None, change: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "<!-- t:start -->";
    const E: &str = "<!-- t:end -->";

    fn render(eol: &str) -> String {
        [S, "## Block", "- line", E].join(eol)
    }

    fn spec<'a>(r: &'a dyn Fn(&str) -> String) -> BlockSpec<'a> {
        BlockSpec { start: S, end: E, render: r, max_block_bytes: 4096, legacy_hashes: &[], is_already_pointing: None }
    }

    fn apply(current: &str) -> (BlockAction, String) {
        let r = render;
        let edit = plan_block_edit(&spec(&r), Some(current.as_bytes()));
        let out = edit.desired.map(|d| String::from_utf8(d).unwrap()).unwrap_or_else(|| current.to_string());
        (edit.action, out)
    }

    #[test]
    fn create_append_replace_noop() {
        let r = render;
        let created = plan_block_edit(&spec(&r), None);
        assert_eq!(created.action, BlockAction::Create);
        let content = String::from_utf8(created.desired.unwrap()).unwrap();
        assert_eq!(apply(&content).0, BlockAction::Noop);

        let (a, appended) = apply("# Mine\nkeep me");
        assert_eq!(a, BlockAction::Update);
        assert!(appended.starts_with("# Mine\nkeep me\n\n<!-- t:start -->"));
        assert_eq!(apply(&appended).0, BlockAction::Noop, "idempotent");

        let stale = appended.replace("- line", "- old");
        let (a, replaced) = apply(&stale);
        assert_eq!(a, BlockAction::Update);
        assert_eq!(replaced, appended);
    }

    #[test]
    fn crlf_is_preserved() {
        let (_, out) = apply("# Mine\r\nkeep\r\n");
        assert!(out.starts_with("# Mine\r\nkeep\r\n\r\n<!-- t:start -->\r\n## Block\r\n"));
        assert!(!out.replace("\r\n", "").contains('\n'));
        assert_eq!(apply(&out).0, BlockAction::Noop);
    }

    #[test]
    fn malformed_markers_conflict() {
        for bad in [
            format!("{}\nx\n", S),
            format!("{}\n{}\n{}\n{}\n", S, E, S, E),
            format!("{}\n{}\n", E, S),
            format!("prefix {}\n{}\n", S, E),
        ] {
            assert_eq!(apply(&bad).0, BlockAction::Conflict, "{:?}", bad);
        }
        let r = render;
        let e = plan_block_edit(&spec(&r), Some(&[0xff, 0xfe, 0x00]));
        assert_eq!(e.reason, BlockReason::InvalidEncoding);
    }

    #[test]
    fn legacy_hash_and_pointing() {
        let r = render;
        let legacy = "old knobyte file\n";
        let hash = sha256_hex(legacy.as_bytes());
        let hashes = [hash.as_str()];
        let s = BlockSpec { legacy_hashes: &hashes, ..spec(&r) };
        let e = plan_block_edit(&s, Some(legacy.as_bytes()));
        assert_eq!(e.action, BlockAction::Migrate);
        let p = |c: &str| c.contains(".knobyte/");
        let s = BlockSpec { is_already_pointing: Some(&p), ..spec(&r) };
        let e = plan_block_edit(&s, Some(b"read .knobyte/ROUTER.md\n"));
        assert_eq!(e.action, BlockAction::Noop);
    }
}
