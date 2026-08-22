//! Claim extraction from scaffold markdown: paths, commands, dependencies and versions.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::drift::markdown::{self, MarkdownDoc};
use crate::drift::types::{Claim, ClaimKind};

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($pat).unwrap())
        }
    };
}

re!(
    known_extensions,
    r"\.(ts|js|tsx|jsx|py|go|rs|rb|java|swift|json|yaml|yml|toml|md|css|scss|html|vue|svelte|sh)$"
);
re!(
    command_prefixes,
    r"^(npm|yarn|pnpm|bun|make|cargo|python|pip|go|node|npx|tsx|swift)\s"
);
re!(
    dependency_sections,
    r"(?i)key\s*libraries|core\s*technologies|dependencies|stack|tech"
);
re!(template_placeholder, r"[<>\[\]{}]");
re!(
    http_method_prefix,
    r"^(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+/"
);
re!(ip_or_cidr, r"^(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?$");
re!(extension_only, r"^\.[A-Za-z0-9]+$");
re!(
    shell_command_prefix,
    r"^(?:sudo\s+)?(?:ls|cd|cat|grep|find|kubectl|helm|docker|git)\s+"
);
re!(
    dotted_key_with_slash,
    r"^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+/[A-Za-z0-9_.-]+$"
);
re!(package_name, r"^@?[A-Za-z0-9][A-Za-z0-9._/-]*$");
re!(code_like, r"[=();,]");
re!(
    negated_text,
    r"(?i)\b(?:deleted|removed|dropped|retired|orphaned|unreferenced|absent|no longer|does not exist|never created)\b"
);
re!(version_in_strong, r"^(.+?)\s+[v^~>=<]*(\d[\d.]*\S*)$");

/// True when a passage describes a reference as deleted or deliberately absent.
pub fn is_negated_text(text: &str) -> bool {
    negated_text().is_match(text)
}

/// True when a heading suggests its section lists things that do not exist / are not used.
pub fn is_negated_section(heading: Option<&str>) -> bool {
    let Some(h) = heading else { return false };
    let lower = h.to_lowercase();
    [
        "not exist",
        "not use",
        "deliberately not",
        "excluded",
        "removed",
        "deprecated",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// Things that look like paths but are code snippets, URL routes or other non-path content.
pub fn is_not_a_path(value: &str) -> bool {
    (value.starts_with('/') && !known_extensions().is_match(value))
        || http_method_prefix().is_match(value)
        || ip_or_cidr().is_match(value)
        || extension_only().is_match(value)
        || shell_command_prefix().is_match(value)
        || dotted_key_with_slash().is_match(value)
        || code_like().is_match(value)
        || value.contains('"')
        || value.contains('\'')
        || value.contains("..")
        || value.starts_with('~')
        || value.contains('*')
        || value.contains('?')
        || value.chars().any(char::is_whitespace)
}

/// Extract all claims from the markdown file at `path`. `source` is the project-relative path
/// recorded on each claim.
pub fn extract_claims(path: &Path, source: &str) -> Vec<Claim> {
    match fs::read_to_string(path) {
        Ok(content) => extract_claims_from_str(&content, source),
        Err(_) => Vec::new(),
    }
}

/// Extract all claims from markdown `content`.
pub fn extract_claims_from_str(content: &str, source: &str) -> Vec<Claim> {
    let doc = markdown::parse(content);
    let mut claims = Vec::new();
    let negated_codes = negated_by_context(&doc);

    let claim =
        |kind: ClaimKind, value: &str, line: usize, section: Option<&str>, negated: bool| Claim {
            kind,
            value: value.to_string(),
            source: source.to_string(),
            line,
            section: section.map(str::to_string),
            negated,
        };

    // Inline code: path and command claims.
    for (idx, code) in doc.inline_codes.iter().enumerate() {
        let heading = doc.heading_at_line(code.line);
        // A package named inside a dependency entry is not a file.
        if code.in_lead_strong && heading.is_some_and(|h| dependency_sections().is_match(h)) {
            continue;
        }
        let negated = is_negated_section(heading) || negated_codes.contains(&idx);
        let value = code.value.as_str();
        let is_cmd = command_prefixes().is_match(value);

        if (value.contains('/') || known_extensions().is_match(value))
            && !is_cmd
            && !template_placeholder().is_match(value)
            && !is_not_a_path(value)
        {
            claims.push(claim(ClaimKind::Path, value, code.line, heading, negated));
        }
        if is_cmd {
            claims.push(claim(
                ClaimKind::Command,
                value,
                code.line,
                heading,
                negated,
            ));
        }
    }

    // Fenced code blocks: each line may be a command.
    for block in &doc.code_blocks {
        let heading = doc.heading_at_line(block.line);
        let negated = is_negated_section(heading);
        for line in &block.lines {
            let trimmed = line.trim();
            if command_prefixes().is_match(trimmed) {
                claims.push(claim(
                    ClaimKind::Command,
                    trimmed,
                    block.line,
                    heading,
                    negated,
                ));
            }
        }
    }

    // Dependencies: bold that opens a list item in a dependency section.
    for strong in &doc.lead_strongs {
        let heading = doc.heading_at_line(strong.line);
        let Some(h) = heading else { continue };
        if !dependency_sections().is_match(h) {
            continue;
        }
        let negated = is_negated_section(heading);
        if !strong.codes.is_empty() {
            for code in &strong.codes {
                claims.push(claim(
                    ClaimKind::Dependency,
                    code,
                    strong.line,
                    heading,
                    negated,
                ));
            }
            continue;
        }
        let text = strong.text.trim();
        if text.is_empty() {
            continue;
        }
        let version = version_in_strong().captures(text);
        let name = version
            .as_ref()
            .map(|c| c.get(1).unwrap().as_str().trim())
            .unwrap_or(text);
        if !package_name().is_match(name) {
            continue;
        }
        claims.push(claim(
            ClaimKind::Dependency,
            name,
            strong.line,
            heading,
            negated,
        ));
        if version.is_some() {
            claims.push(claim(
                ClaimKind::Version,
                text,
                strong.line,
                heading,
                negated,
            ));
        }
    }

    claims
}

/// Indexes of inline code spans whose own sentence describes them as deleted or absent.
fn negated_by_context(doc: &MarkdownDoc) -> HashSet<usize> {
    let mut out = HashSet::new();
    for (pidx, para) in doc.paragraphs.iter().enumerate() {
        let chars: Vec<char> = para.text.chars().collect();
        // Sentence boundaries: `.`, `!` or `?` followed by whitespace or the end.
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut start = 0;
        for (i, c) in chars.iter().enumerate() {
            if matches!(c, '.' | '!' | '?')
                && chars.get(i + 1).map(|n| n.is_whitespace()).unwrap_or(true)
            {
                spans.push((start, i + 1));
                start = i + 1;
            }
        }
        spans.push((start, chars.len()));

        for (cidx, code) in doc.inline_codes.iter().enumerate() {
            if code.paragraph != Some(pidx) {
                continue;
            }
            let at = code.offset;
            let sentence: String = match spans.iter().find(|(from, to)| at >= *from && at < *to) {
                Some((from, to)) => chars[*from..*to].iter().collect(),
                None => para.text.clone(),
            };
            if is_negated_text(&sentence) {
                out.insert(cidx);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(claims: &[Claim], kind: ClaimKind) -> Vec<String> {
        claims
            .iter()
            .filter(|c| c.kind == kind)
            .map(|c| c.value.clone())
            .collect()
    }

    #[test]
    fn extracts_paths_and_skips_non_paths() {
        let c = extract_claims_from_str(
            "# Notes\n\nSee `src/index.ts`, `pipeline.py`, `/api/users`, `192.168.5.0/24`, `.yaml`, `*_client.py`, `~/.claude/x`, `nodemon src/index.ts`, `patterns/<name>.md`, `.github/CODEOWNERS`, `argocd.argoproj.io/sync-wave`.",
            "t.md",
        );
        assert_eq!(
            kinds(&c, ClaimKind::Path),
            vec!["src/index.ts", "pipeline.py", ".github/CODEOWNERS"]
        );
    }

    #[test]
    fn negation_by_section_and_sentence() {
        let c = extract_claims_from_str(
            "# What Does NOT Exist\n\nWe don't have `src/admin/` yet.\n\n# Files\n\nWe deleted `old.ts`. Keep `new.ts` though.\n",
            "t.md",
        );
        let paths: Vec<_> = c.iter().filter(|c| c.kind == ClaimKind::Path).collect();
        assert!(paths[0].negated);
        assert!(paths[1].negated, "{:?}", paths[1]);
        assert!(!paths[2].negated);
    }

    #[test]
    fn commands_from_inline_and_blocks() {
        let c = extract_claims_from_str(
            "# Setup\n\nRun `npm run build`, `yarn test`, `make deploy`.\n\n```sh\nnpm install\nnpm run dev\n```\n",
            "t.md",
        );
        assert_eq!(
            kinds(&c, ClaimKind::Command),
            vec![
                "npm run build",
                "yarn test",
                "make deploy",
                "npm install",
                "npm run dev"
            ]
        );
    }

    #[test]
    fn dependencies_and_versions() {
        let c = extract_claims_from_str(
            "# Core Technologies\n\n- **React 18** — UI\n- **Node v20** — runtime\n- **Supabase (Postgres + Auth)** — db\n- **Express 4.21 on Node** — api\n- **Groq (`groq-sdk`)** — llm; the **service-role** key\n- **YouTube via `youtubei.js`** — transcripts\n\n# Architecture\n\n**Important** note and `src/index.ts`.\n",
            "t.md",
        );
        assert_eq!(
            kinds(&c, ClaimKind::Dependency),
            vec!["React", "Node", "groq-sdk", "youtubei.js"]
        );
        assert_eq!(kinds(&c, ClaimKind::Version), vec!["React 18", "Node v20"]);
        assert_eq!(kinds(&c, ClaimKind::Path), vec!["src/index.ts"]);
    }

    #[test]
    fn multiword_phrases_are_not_dependencies() {
        let c = extract_claims_from_str(
            "## Tech Stack\n\n- **REST API** — external\n- **Database Layer** — persistence\n- **pino-http** — logs\n",
            "t.md",
        );
        assert_eq!(kinds(&c, ClaimKind::Dependency), vec!["pino-http"]);
    }
}
