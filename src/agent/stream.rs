//! Decoding the JSONL event streams of headless Claude Code (`--output-format stream-json`)
//! and Codex (`codex exec --json`) into a small transcript / activity vocabulary.
//!
//! Only allowlisted fields cross this boundary: assistant prose, tool names, and the file path
//! or command a tool acted on, all sanitized of control characters and bounded in length.

use serde::Serialize;

use super::AgentTool;

/// Upper bound on one transcript entry.
pub const MAX_ENTRY_CHARS: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Reading,
    Searching,
    Writing,
    RunningCommand,
    Delegating,
    Working,
}

impl ActivityKind {
    pub fn label(&self) -> &'static str {
        match self {
            ActivityKind::Reading => "read",
            ActivityKind::Searching => "search",
            ActivityKind::Writing => "write",
            ActivityKind::RunningCommand => "run",
            ActivityKind::Delegating => "delegate",
            ActivityKind::Working => "tool",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The agent session started.
    Started,
    /// Assistant prose.
    Assistant { text: String },
    /// The agent invoked a tool.
    Tool { kind: ActivityKind, detail: String },
    /// A tool call failed.
    ToolFailed { detail: String },
    /// The agent reported successful completion.
    Completed,
    /// The agent reported failure.
    Failed { detail: String },
}

/// Strip ANSI escapes and control characters (keeping newlines and tabs), bound the length.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_ENTRY_CHARS));
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ ... final byte in @..~ ; OSC: ESC ] ... BEL or ESC \
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' || (n == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        if out.chars().count() >= MAX_ENTRY_CHARS {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

fn claude_tool_kind(name: &str) -> ActivityKind {
    match name {
        "Read" => ActivityKind::Reading,
        "Glob" | "Grep" | "WebSearch" | "WebFetch" => ActivityKind::Searching,
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => ActivityKind::Writing,
        "Bash" | "PowerShell" => ActivityKind::RunningCommand,
        "Task" | "Agent" => ActivityKind::Delegating,
        _ => ActivityKind::Working,
    }
}

fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// Stateful decoder for one agent session.
pub struct StreamDecoder {
    tool: AgentTool,
    completed: bool,
    failed: bool,
    seen_items: std::collections::HashSet<String>,
}

impl StreamDecoder {
    pub fn new(tool: AgentTool) -> Self {
        Self { tool, completed: false, failed: false, seen_items: Default::default() }
    }

    /// Whether the agent reported successful completion (and no failure).
    pub fn completed(&self) -> bool {
        self.completed && !self.failed
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Decode one JSONL line. Non-JSON lines are ignored.
    pub fn feed_line(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            return Vec::new();
        };
        if !event.is_object() {
            return Vec::new();
        }
        match self.tool {
            AgentTool::Claude => self.claude(&event),
            AgentTool::Codex => self.codex(&event),
        }
    }

    fn fail(&mut self, detail: &str) -> AgentEvent {
        self.failed = true;
        AgentEvent::Failed { detail: sanitize(detail) }
    }

    fn claude(&mut self, e: &serde_json::Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match str_field(e, "type") {
            Some("system") if str_field(e, "subtype") == Some("init") => {
                self.completed = false;
                out.push(AgentEvent::Started);
            }
            Some("result") => {
                let is_error = e.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                let subtype = str_field(e, "subtype").unwrap_or("");
                if is_error || subtype.starts_with("error") {
                    out.push(self.fail(subtype));
                } else if subtype == "success" && !self.failed {
                    self.completed = true;
                    out.push(AgentEvent::Completed);
                }
            }
            Some(kind @ ("assistant" | "user")) => {
                let Some(content) = e.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) else {
                    return out;
                };
                for block in content.iter().take(64) {
                    match (kind, str_field(block, "type")) {
                        ("assistant", Some("text")) => {
                            if let Some(t) = str_field(block, "text") {
                                let t = sanitize(t);
                                if !t.trim().is_empty() {
                                    out.push(AgentEvent::Assistant { text: t });
                                }
                            }
                        }
                        ("assistant", Some("tool_use")) => {
                            let name = str_field(block, "name").unwrap_or("tool");
                            let input = block.get("input").cloned().unwrap_or_default();
                            let target = str_field(&input, "file_path")
                                .or_else(|| str_field(&input, "path"))
                                .or_else(|| str_field(&input, "command"))
                                .or_else(|| str_field(&input, "pattern"))
                                .or_else(|| str_field(&input, "description"))
                                .unwrap_or("");
                            let detail = if target.is_empty() {
                                name.to_string()
                            } else {
                                format!("{} {}", name, first_line(target))
                            };
                            out.push(AgentEvent::Tool { kind: claude_tool_kind(name), detail: sanitize(&detail) });
                        }
                        ("user", Some("tool_result"))
                            if block.get("is_error").and_then(|v| v.as_bool()) == Some(true) =>
                        {
                            out.push(AgentEvent::ToolFailed { detail: "tool call failed".to_string() });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn codex(&mut self, e: &serde_json::Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match str_field(e, "type") {
            Some("thread.started") => {
                self.completed = false;
                out.push(AgentEvent::Started);
            }
            Some("turn.started") => self.completed = false,
            Some("turn.completed") => {
                if !self.failed {
                    self.completed = true;
                    out.push(AgentEvent::Completed);
                }
            }
            Some("turn.failed") => {
                let msg = e.get("error").and_then(|x| str_field(x, "message")).unwrap_or("turn failed").to_string();
                out.push(self.fail(&msg));
            }
            Some("error") => {
                let msg = str_field(e, "message").unwrap_or("error").to_string();
                out.push(self.fail(&msg));
            }
            Some(phase @ ("item.started" | "item.updated" | "item.completed")) => {
                let Some(item) = e.get("item") else { return out };
                let id = str_field(item, "id").unwrap_or("").to_string();
                let key = |suffix: &str| format!("{}:{}", id, suffix);
                match str_field(item, "type") {
                    Some("agent_message") if phase == "item.completed" => {
                        if let Some(t) = str_field(item, "text") {
                            out.push(AgentEvent::Assistant { text: sanitize(t) });
                        }
                    }
                    Some("command_execution") => {
                        if self.seen_items.insert(key("cmd")) {
                            let cmd = str_field(item, "command").unwrap_or("command");
                            out.push(AgentEvent::Tool { kind: ActivityKind::RunningCommand, detail: sanitize(first_line(cmd)) });
                        }
                        let failed = matches!(str_field(item, "status"), Some("failed") | Some("declined"))
                            || item.get("exit_code").and_then(|c| c.as_i64()).is_some_and(|c| c != 0);
                        if phase == "item.completed" && failed {
                            out.push(AgentEvent::ToolFailed { detail: sanitize(first_line(str_field(item, "command").unwrap_or("command"))) });
                        }
                    }
                    Some("file_change") => {
                        if self.seen_items.insert(key("file")) {
                            let paths: Vec<&str> = item
                                .get("changes")
                                .and_then(|c| c.as_array())
                                .map(|a| a.iter().take(16).filter_map(|c| str_field(c, "path")).collect())
                                .unwrap_or_default();
                            out.push(AgentEvent::Tool { kind: ActivityKind::Writing, detail: sanitize(&paths.join(", ")) });
                        }
                    }
                    Some("web_search") if self.seen_items.insert(key("web")) => {
                        out.push(AgentEvent::Tool { kind: ActivityKind::Searching, detail: sanitize(str_field(item, "query").unwrap_or("web search")) });
                    }
                    Some("mcp_tool_call") if self.seen_items.insert(key("mcp")) => {
                        out.push(AgentEvent::Tool { kind: ActivityKind::Working, detail: sanitize(str_field(item, "tool").unwrap_or("mcp tool")) });
                    }
                    Some("collab_tool_call") if self.seen_items.insert(key("collab")) => {
                        out.push(AgentEvent::Tool { kind: ActivityKind::Delegating, detail: "sub-agent".to_string() });
                    }
                    _ => {}
                }
                if self.seen_items.len() > 4096 {
                    self.seen_items.clear();
                }
            }
            _ => {}
        }
        out
    }
}

fn first_line(s: &str) -> &str {
    let line = s.lines().next().unwrap_or("");
    match line.char_indices().nth(200) {
        Some((i, _)) => &line[..i],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_stream() {
        let mut d = StreamDecoder::new(AgentTool::Claude);
        assert_eq!(d.feed_line(r#"{"type":"system","subtype":"init"}"#), vec![AgentEvent::Started]);
        let ev = d.feed_line(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reading \u001b[31mfiles"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":".knobyte/context/stack.md"}}]}}"#);
        assert_eq!(ev[0], AgentEvent::Assistant { text: "Reading files".into() });
        assert_eq!(ev[1], AgentEvent::Tool { kind: ActivityKind::Writing, detail: "Edit .knobyte/context/stack.md".into() });
        assert!(!d.completed());
        assert_eq!(d.feed_line(r#"{"type":"result","subtype":"success","is_error":false}"#), vec![AgentEvent::Completed]);
        assert!(d.completed());
        assert!(d.feed_line("not json").is_empty());
    }

    #[test]
    fn codex_stream() {
        let mut d = StreamDecoder::new(AgentTool::Codex);
        d.feed_line(r#"{"type":"thread.started","thread_id":"x"}"#);
        let ev = d.feed_line(r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"knobyte graph scope auth","status":"in_progress"}}"#);
        assert_eq!(ev.len(), 1);
        assert!(d.feed_line(r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"knobyte graph scope auth","exit_code":0,"status":"completed"}}"#).is_empty());
        let ev = d.feed_line(r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"done"}}"#);
        assert_eq!(ev, vec![AgentEvent::Assistant { text: "done".into() }]);
        d.feed_line(r#"{"type":"turn.completed"}"#);
        assert!(d.completed());
        let mut f = StreamDecoder::new(AgentTool::Codex);
        f.feed_line(r#"{"type":"turn.failed","error":{"message":"boom"}}"#);
        f.feed_line(r#"{"type":"turn.completed"}"#);
        assert!(!f.completed() && f.failed());
    }
}
