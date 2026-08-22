//! `knobyte pattern add <name>`: create a pattern file from the template (never overwriting)
//! and list it in `patterns/INDEX.md`.

use std::fs;
use std::path::PathBuf;

use crate::config::KnobyteConfig;

#[derive(Debug, Clone)]
pub struct PatternAdded {
    pub path: PathBuf,
    /// Whether a row was appended to `patterns/INDEX.md`.
    pub indexed: bool,
}

/// Valid names: letters, digits and hyphens (no leading/trailing hyphen), at most 64 chars.
pub fn validate_pattern_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "Invalid pattern name '{}'. Use only letters, numbers, and hyphens (for example add-endpoint).",
            name
        ))
    }
}

pub fn render_pattern(name: &str, today: &str) -> String {
    let title = name
        .split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"---
id: kb_pattern_{id}
name: {name}
title: {title}
type: pattern
description: "[one line: what this pattern covers and when to use it]"
status: in_flight
revision: 1
triggers:
  - "[keyword that should trigger loading this file]"
relations:
  - type: related_to
    target_id: kb_conventions
    note: when verifying this task
grounds_to: []
last_updated: {today}
---

# {title}

## Context
[What to load or know before starting this task type]

## Steps
[The workflow: what to do, in what order]

## Gotchas
[What goes wrong and what to watch out for]

## Verify
[Checklist to run after completing this task type]

## Debug
[What to check when this task type breaks]

## Update Scaffold
- [ ] Update "Current Project State" in ROUTER.md if what works or is not built changed
- [ ] Update any context file that is now out of date
- [ ] New recurring task type without a pattern? Create one and add it to INDEX.md
"#,
        id = name.to_ascii_lowercase().replace('-', "_"),
        name = name,
        title = title,
        today = today
    )
}

pub fn add_pattern(config: &KnobyteConfig, name: &str) -> Result<PatternAdded, String> {
    validate_pattern_name(name)?;
    let dir = config.patterns_dir();
    let path = dir.join(format!("{}.md", name));
    if path.exists() {
        return Err(format!("Pattern '{}' already exists at {}", name, path.display()));
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    // create_new: never clobber a file created concurrently.
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("{}: {}", path.display(), e))?;
        f.write_all(render_pattern(name, &today).as_bytes()).map_err(|e| e.to_string())?;
    }
    let index = dir.join("INDEX.md");
    let mut indexed = false;
    if let Ok(current) = fs::read_to_string(&index) {
        let prefix = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        let eol = if current.contains("\r\n") { "\r\n" } else { "\n" };
        let row = format!("{}| [{name}.md]({name}.md) | [description] |{}", prefix, eol, name = name);
        let mut f = fs::OpenOptions::new().append(true).open(&index).map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut f, row.as_bytes()).map_err(|e| e.to_string())?;
        indexed = true;
    }
    Ok(PatternAdded { path, indexed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_validates_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let c = KnobyteConfig::new(d.path().to_path_buf(), d.path().join(".knobyte"));
        fs::create_dir_all(c.patterns_dir()).unwrap();
        fs::write(c.patterns_dir().join("INDEX.md"), "| Pattern | Use when |\n|---|---|").unwrap();
        assert!(add_pattern(&c, "../evil").is_err());
        assert!(add_pattern(&c, "has space").is_err());
        let added = add_pattern(&c, "add-endpoint").unwrap();
        assert!(added.indexed);
        let index = fs::read_to_string(c.patterns_dir().join("INDEX.md")).unwrap();
        assert!(index.ends_with("|---|---|\n| [add-endpoint.md](add-endpoint.md) | [description] |\n"));
        assert!(fs::read_to_string(&added.path).unwrap().contains("last_updated: "));
        assert!(add_pattern(&c, "add-endpoint").unwrap_err().contains("already exists"));
    }
}
