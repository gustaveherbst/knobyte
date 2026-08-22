//! Launching Claude Code or Codex on the user's behalf (`knobyte setup`, `knobyte sync`).
//!
//! Safety contract:
//! - A launch only happens when the user runs setup/sync interactively and confirms, or passes
//!   an explicit flag (`--launch-agent`). Non-interactive runs and CI (`CI` set) never launch
//!   without the flag, and `KNOBYTE_NO_AGENT_LAUNCH=1` disables launching entirely.
//! - The exact command line and the pre-approved tool list are shown before launching.
//! - The agent runs headless in its own process group; Ctrl-C, SIGTERM or the timeout kill the
//!   whole tree (SIGTERM, then SIGKILL after a grace period), and leftover helpers are reaped
//!   after a normal exit.
//! - The prompt is written to a private file under `.knobyte/local/` (removed afterwards) and
//!   the agent is told to read it, so long prompts never hit the argument list.
//! - When no selected agent CLI is installed, callers print the prompt for manual pasting.

pub mod stream;
pub mod sync;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use colored::Colorize;
use serde::Serialize;

pub use stream::{ActivityKind, AgentEvent, StreamDecoder};

/// Agents Knobyte can launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTool {
    Claude,
    Codex,
}

impl AgentTool {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Some(AgentTool::Claude),
            "codex" => Some(AgentTool::Codex),
            _ => None,
        }
    }
    pub fn id(&self) -> &'static str {
        match self {
            AgentTool::Claude => "claude",
            AgentTool::Codex => "codex",
        }
    }
    /// Executable name looked up on PATH.
    pub fn program(&self) -> &'static str {
        self.id()
    }
    pub fn display_name(&self) -> &'static str {
        match self {
            AgentTool::Claude => "Claude Code",
            AgentTool::Codex => "Codex",
        }
    }
}

/// Read-only Knobyte commands a headless Claude session may run without asking. Maintenance,
/// team and wiki-writing commands still need a person.
pub const HEADLESS_KNOBYTE_COMMANDS: &[&str] = &[
    "knobyte graph scope",
    "knobyte graph get",
    "knobyte graph query",
    "knobyte graph status",
    "knobyte impact",
    "knobyte init",
    "knobyte logging",
    "knobyte log",
    "knobyte timeline",
    "knobyte capabilities",
];

/// The `--allowedTools` entries for headless Claude (Bash and PowerShell spellings).
pub fn allowed_tools() -> Vec<String> {
    HEADLESS_KNOBYTE_COMMANDS
        .iter()
        .flat_map(|c| [format!("Bash({}:*)", c), format!("PowerShell({}:*)", c)])
        .collect()
}

/// A program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentCommand {
    pub program: String,
    pub args: Vec<String>,
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=,@%+".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

impl AgentCommand {
    /// Copy/paste-safe rendering of the exact command line.
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Build the headless invocation for `tool`.
pub fn build_agent_command(tool: AgentTool, instruction: &str, allow_non_git: bool) -> AgentCommand {
    match tool {
        AgentTool::Claude => AgentCommand {
            program: tool.program().to_string(),
            args: vec![
                "-p".into(),
                instruction.into(),
                "--permission-mode".into(),
                "acceptEdits".into(),
                "--allowedTools".into(),
                allowed_tools().join(","),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
            ],
        },
        AgentTool::Codex => {
            let mut args: Vec<String> = vec!["exec".into(), "--json".into(), "--sandbox".into(), "workspace-write".into()];
            if allow_non_git {
                args.push("--skip-git-repo-check".into());
            }
            args.push(instruction.into());
            AgentCommand { program: tool.program().to_string(), args }
        }
    }
}

/// Resolve `program` on `path` (or the process PATH).
pub fn find_on_path(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let owned;
    let path = match path {
        Some(p) => p,
        None => {
            owned = std::env::var_os("PATH")?;
            owned.as_os_str()
        }
    };
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        if cfg!(windows) {
            for ext in ["exe", "cmd", "bat"] {
                let c = dir.join(format!("{}.{}", program, ext));
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// The first selected tool (in the user's order) that is a launchable agent installed on PATH.
pub fn select_agent(selected_tools: &[String], path: Option<&OsStr>) -> Option<AgentTool> {
    selected_tools
        .iter()
        .filter_map(|t| AgentTool::parse(t))
        .find(|t| find_on_path(t.program(), path).is_some())
}

/// Every launchable agent installed on PATH.
pub fn installed_agents(path: Option<&OsStr>) -> Vec<AgentTool> {
    [AgentTool::Claude, AgentTool::Codex]
        .into_iter()
        .filter(|t| find_on_path(t.program(), path).is_some())
        .collect()
}

/// Whether a launch may even be offered. `interactive`: stdin and stdout are terminals and the
/// user can confirm. `explicit`: the user passed `--launch-agent`.
pub fn launch_permitted(interactive: bool, explicit: bool) -> Result<(), String> {
    if std::env::var_os("KNOBYTE_NO_AGENT_LAUNCH").is_some_and(|v| !v.is_empty() && v != "0") {
        return Err("agent launch is disabled by KNOBYTE_NO_AGENT_LAUNCH".into());
    }
    if explicit {
        return Ok(());
    }
    if std::env::var_os("CI").is_some_and(|v| !v.is_empty() && v != "0" && v != "false") {
        return Err("running in CI; pass --launch-agent to launch an agent".into());
    }
    if !interactive {
        return Err("not an interactive terminal; pass --launch-agent to launch an agent".into());
    }
    Ok(())
}

/// Whether stdin and stdout are both terminals.
pub fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Ask a yes/no question on the terminal. Empty answers take `default`. Non-interactive
/// sessions always answer `false`.
pub fn confirm(question: &str, default: bool) -> bool {
    if !is_interactive() {
        return false;
    }
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{} {} ", question, hint);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}

/// Read one line from the terminal (trimmed); `None` when not interactive or on EOF.
pub fn prompt_line(question: &str) -> Option<String> {
    if !is_interactive() {
        return None;
    }
    print!("{}", question);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// Print the exact launch preview: command, working directory and pre-approved tools.
pub fn print_launch_preview(tool: AgentTool, command: &AgentCommand, cwd: &Path) {
    println!("{}", format!("{} will run in {}:", tool.display_name(), cwd.display()).bold());
    println!("    {}", command.display());
    if tool == AgentTool::Claude {
        println!("  Pre-approved commands (everything else needs your approval or is denied):");
        for c in HEADLESS_KNOBYTE_COMMANDS {
            println!("    - {}", c);
        }
        println!("  File edits are accepted automatically (--permission-mode acceptEdits).");
    } else {
        println!("  Codex runs non-interactively with a workspace-write sandbox.");
    }
    println!("  Press Ctrl-C to stop the agent and every process it started.");
}

/// Why a launch did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchFailure {
    /// The CLI could not be started.
    Launch,
    /// The CLI rejected its arguments (outdated CLI).
    Arguments,
    /// The CLI is not signed in.
    Authentication,
    /// Non-zero exit or a reported failure.
    Failed,
    /// Exited 0 without confirming completion.
    Protocol,
    /// Ctrl-C / SIGTERM.
    Cancelled,
    /// The time limit was exceeded.
    Timeout,
    /// The private prompt file could not be prepared.
    Prompt,
}

impl LaunchFailure {
    pub fn message(&self, tool: AgentTool) -> String {
        let name = tool.display_name();
        match self {
            LaunchFailure::Launch => format!("{} could not start. Check its installation and try again.", name),
            LaunchFailure::Arguments => format!("{} rejected the headless command. Update the CLI and try again.", name),
            LaunchFailure::Authentication => format!("{} could not authenticate. Sign in to its CLI and try again.", name),
            LaunchFailure::Failed => format!("{} exited before finishing. Check its CLI configuration and try again.", name),
            LaunchFailure::Protocol => format!("{} stopped without confirming completion. Review the files and rerun.", name),
            LaunchFailure::Cancelled => "The agent session was cancelled; its process tree was stopped.".to_string(),
            LaunchFailure::Timeout => "The agent session exceeded its time limit and was stopped.".to_string(),
            LaunchFailure::Prompt => "The private prompt file could not be prepared or removed safely.".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LaunchOutcome {
    pub tool: AgentTool,
    pub completed: bool,
    pub failure: Option<LaunchFailure>,
    pub exit_code: Option<i32>,
    /// Bounded record of what the agent said and did.
    pub transcript: Vec<AgentEvent>,
}

pub struct LaunchOptions {
    /// Working directory (the project root).
    pub cwd: PathBuf,
    /// Directory for the private prompt file (normally `.knobyte/local`).
    pub private_dir: PathBuf,
    pub timeout: Option<Duration>,
    /// PATH for lookup and for the child (tests point this at a fake CLI).
    pub path_env: Option<OsString>,
    /// Pass `--skip-git-repo-check` to Codex (agent-memory workspaces outside git).
    pub allow_non_git: bool,
}

const TERMINATE_GRACE: Duration = Duration::from_millis(500);
const MAX_TRANSCRIPT: usize = 2000;
const DIAGNOSTIC_BYTES: usize = 8 * 1024;

static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Ask the agent session currently running in this process (if any) to stop, exactly as
/// Ctrl-C would: its process tree is terminated and the outcome reports `Cancelled`.
pub fn request_cancel() {
    CANCELLED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
}

/// Installs SIGINT/SIGTERM handlers for the life of the guard, restoring the previous ones.
struct SignalGuard {
    #[cfg(unix)]
    previous: Vec<(libc::c_int, libc::sighandler_t)>,
}

impl SignalGuard {
    fn install() -> Self {
        CANCELLED.store(false, Ordering::SeqCst);
        #[cfg(unix)]
        {
            let mut previous = Vec::new();
            for sig in [libc::SIGINT, libc::SIGTERM] {
                // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
                let prev = unsafe { libc::signal(sig, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t) };
                previous.push((sig, prev));
            }
            SignalGuard { previous }
        }
        #[cfg(not(unix))]
        {
            SignalGuard {}
        }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        for (sig, prev) in &self.previous {
            // SAFETY: restoring the handler that was installed before.
            unsafe {
                libc::signal(*sig, *prev);
            }
        }
    }
}

#[cfg(unix)]
fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// Terminate the child's whole process group: SIGTERM, a grace period, then SIGKILL.
fn terminate_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        if group_alive(pgid) {
            // SAFETY: the child was spawned as the leader of its own process group.
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
            let deadline = Instant::now() + TERMINATE_GRACE;
            while Instant::now() < deadline {
                let _ = child.try_wait();
                if !group_alive(pgid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if group_alive(pgid) {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn classify(stderr: &str, stdout_tail: &str) -> LaunchFailure {
    let text = format!("{}\n{}", stderr, stdout_tail).to_ascii_lowercase();
    if ["unexpected argument", "unknown option", "unknown argument", "unrecognized option", "unrecognized argument"]
        .iter()
        .any(|p| text.contains(p))
    {
        LaunchFailure::Arguments
    } else if ["not logged in", "not signed in", "authentication failed", "authentication required", "invalid api key", "unauthorized", "please log in", "please sign in"]
        .iter()
        .any(|p| text.contains(p))
    {
        LaunchFailure::Authentication
    } else {
        LaunchFailure::Failed
    }
}

/// Write `prompt` to a private file and return (session dir, instruction pointing at it).
fn prepare_prompt(prompt: &str, opts: &LaunchOptions) -> Result<(PathBuf, String), LaunchFailure> {
    let dir = opts.private_dir.join("agent-sessions").join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&dir).map_err(|_| LaunchFailure::Prompt)?;
    let file = dir.join("prompt.md");
    {
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&file).map_err(|_| LaunchFailure::Prompt)?;
        std::io::Write::write_all(&mut f, prompt.as_bytes()).map_err(|_| LaunchFailure::Prompt)?;
    }
    let pointer = file
        .strip_prefix(&opts.cwd)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| file.to_string_lossy().to_string());
    Ok((dir, format!("Read the full prompt from `{}`, then follow it exactly.", pointer)))
}

/// The command [`run_agent`] would execute for `prompt` (with a placeholder prompt path), for
/// previews before the private prompt file exists.
pub fn preview_command(tool: AgentTool, opts: &LaunchOptions) -> AgentCommand {
    let pointer = opts
        .private_dir
        .join("agent-sessions/<session>/prompt.md")
        .strip_prefix(&opts.cwd)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| ".knobyte/local/agent-sessions/<session>/prompt.md".into());
    build_agent_command(tool, &format!("Read the full prompt from `{}`, then follow it exactly.", pointer), opts.allow_non_git)
}

/// Run `tool` headless on `prompt`, streaming decoded events to `on_event`. Blocks until the
/// agent exits, fails, is cancelled (Ctrl-C) or times out; the process tree is always reaped.
pub fn run_agent(
    tool: AgentTool,
    prompt: &str,
    opts: &LaunchOptions,
    on_event: &mut dyn FnMut(&AgentEvent),
) -> LaunchOutcome {
    let mut outcome = LaunchOutcome { tool, completed: false, failure: None, exit_code: None, transcript: Vec::new() };
    let Some(program) = find_on_path(tool.program(), opts.path_env.as_deref()) else {
        outcome.failure = Some(LaunchFailure::Launch);
        return outcome;
    };
    let (session_dir, instruction) = match prepare_prompt(prompt, opts) {
        Ok(v) => v,
        Err(f) => {
            outcome.failure = Some(f);
            return outcome;
        }
    };
    let command = build_agent_command(tool, &instruction, opts.allow_non_git);

    let mut cmd = Command::new(&program);
    cmd.args(&command.args)
        .current_dir(&opts.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(p) = &opts.path_env {
        cmd.env("PATH", p);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let guard = SignalGuard::install();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            drop(guard);
            let _ = fs::remove_dir_all(&session_dir);
            outcome.failure = Some(LaunchFailure::Launch);
            return outcome;
        }
    };

    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        if let Some(out) = stdout {
            let mut r = BufReader::new(out);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if buf.len() > 256 * 1024 {
                            continue; // oversized record: discarded, never truncated into JSON
                        }
                        if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    let stderr = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut collected = Vec::new();
        if let Some(mut e) = stderr {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = e.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let room = DIAGNOSTIC_BYTES.saturating_sub(collected.len());
                collected.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
        String::from_utf8_lossy(&collected).into_owned()
    });

    let mut decoder = StreamDecoder::new(tool);
    let mut tail = String::new();
    let started = Instant::now();
    let mut handle = |line: String, outcome: &mut LaunchOutcome, decoder: &mut StreamDecoder, tail: &mut String| {
        if tail.len() < DIAGNOSTIC_BYTES {
            tail.push_str(&line);
        }
        for ev in decoder.feed_line(&line) {
            on_event(&ev);
            if outcome.transcript.len() < MAX_TRANSCRIPT {
                outcome.transcript.push(ev);
            }
        }
    };

    let status = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => handle(line, &mut outcome, &mut decoder, &mut tail),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // stdout closed: wait for the exit status (bounded by cancel/timeout below).
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if CANCELLED.load(Ordering::SeqCst) {
            terminate_tree(&mut child);
            outcome.failure = Some(LaunchFailure::Cancelled);
            break None;
        }
        if opts.timeout.is_some_and(|t| started.elapsed() > t) {
            terminate_tree(&mut child);
            outcome.failure = Some(LaunchFailure::Timeout);
            break None;
        }
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => {
                terminate_tree(&mut child);
                outcome.failure = Some(LaunchFailure::Failed);
                break None;
            }
        }
    };

    // Helpers the agent left behind must not keep editing after the session ends.
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        if group_alive(pgid) {
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
            std::thread::sleep(TERMINATE_GRACE);
            if group_alive(pgid) {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
    }
    let _ = reader.join();
    while let Ok(line) = rx.try_recv() {
        handle(line, &mut outcome, &mut decoder, &mut tail);
    }
    let stderr_text = err_reader.join().unwrap_or_default();
    drop(guard);
    if fs::remove_dir_all(&session_dir).is_err() && session_dir.exists() && outcome.failure.is_none() {
        outcome.failure = Some(LaunchFailure::Prompt);
    }

    if let Some(status) = status {
        outcome.exit_code = status.code();
        if outcome.failure.is_none() {
            if !status.success() || decoder.failed() {
                outcome.failure = Some(classify(&stderr_text, &tail));
            } else if !decoder.completed() {
                outcome.failure = Some(LaunchFailure::Protocol);
            } else {
                outcome.completed = true;
            }
        }
    }
    outcome
}

/// Default terminal rendering of an agent event.
pub fn print_event(ev: &AgentEvent) {
    match ev {
        AgentEvent::Started => println!("{}", "  agent session started".dimmed()),
        AgentEvent::Assistant { text } => {
            for line in text.lines() {
                println!("  {}", line);
            }
        }
        AgentEvent::Tool { kind, detail } => println!("  {} {}", format!("[{}]", kind.label()).cyan(), detail.dimmed()),
        AgentEvent::ToolFailed { detail } => println!("  {} {}", "[tool failed]".yellow(), detail.dimmed()),
        AgentEvent::Completed => println!("{}", "  agent reported completion".green()),
        AgentEvent::Failed { detail } => println!("  {} {}", "[agent failed]".red(), detail),
    }
}

/// Print a prompt for pasting into an agent manually.
pub fn print_prompt_for_paste(prompt: &str) {
    println!();
    println!("------------------------- COPY BELOW THIS LINE -------------------------");
    println!();
    println!("{}", prompt);
    println!();
    println!("------------------------- COPY ABOVE THIS LINE -------------------------");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_shapes() {
        let c = build_agent_command(AgentTool::Claude, "do it", false);
        assert_eq!(c.program, "claude");
        assert!(c.args.windows(2).any(|w| w[0] == "--allowedTools" && w[1].contains("Bash(knobyte graph scope:*)")));
        assert!(c.display().contains("'do it'"));
        let x = build_agent_command(AgentTool::Codex, "go", true);
        assert_eq!(x.args[..2], ["exec".to_string(), "--json".to_string()]);
        assert!(x.args.contains(&"--skip-git-repo-check".to_string()));
        assert_eq!(x.args.last().unwrap(), "go");
    }

    #[test]
    fn launch_gate_requires_flag_without_tty() {
        if std::env::var_os("KNOBYTE_NO_AGENT_LAUNCH").is_none() {
            assert!(launch_permitted(false, false).is_err());
            assert!(launch_permitted(false, true).is_ok());
        }
    }
}
