# Getting Started with Knobyte

Official Website: **[https://knobyte.ai](https://knobyte.ai)**

This guide walks you through building Knobyte, initializing it in a software repository, and populating your initial project memory.

---

## 1. Prerequisites & Compilation

Knobyte is written in 100% pure Rust. It requires **Rust 1.75+** and `cargo`. No Node.js runtime, Python virtual environment, or external databases are required.

### Clone and Build

```bash
git clone https://github.com/knobyte-ai/knobyte.git
cd knobyte

# Build optimized release binary
cargo build --release

# Optional: Install globally in your Cargo bin path (~/.cargo/bin)
cargo install --path .
```

The compiled executable is located at `./target/release/knobyte`.

Verify your installation:
```bash
knobyte --help
```

---

## 2. Initializing Knobyte in a Repository

To add Knobyte to any software repository (Rust, TypeScript/JavaScript, Python, Go, etc.):

```bash
cd /path/to/your/project
knobyte setup
```

This creates the **`.knobyte/`** scaffold directory with:
- `config.json`: Repository configuration and settings.
- `AGENTS.md`: High-level project memory policy and non-negotiables for AI agents.
- `ROUTER.md`: Knowledge router table directing agents to relevant context files.
- `.gitignore`: Ensures rebuildable binary databases (`graph.db`, `wiki.db`, `cozo.db`) and local session files are not tracked in Git.
- `context/stack.md`: Starter technology stack specification.
- `events/decisions.jsonl`: Append-only project audit log.

---

## 3. Populating Project Memory

Knobyte uses structured Markdown files in `.knobyte/context/` to describe architecture, conventions, and dependencies.

### Option A: Existing Codebase

Provide the following prompt to your AI coding agent (Cursor, Claude Code, Windsurf, or Cline):

```
You are going to populate an AI context scaffold for this project using Knobyte.
The scaffold lives in .knobyte/ in the root of this repository.

1. Read .knobyte/ROUTER.md and .knobyte/context/stack.md.
2. Explore this codebase:
   - Read the main entry point(s) and project structure.
   - Read representative files from each major subsystem.
3. Populate .knobyte/context/ with real architectural constraints:
   - .knobyte/context/stack.md (languages, frameworks, databases, libraries)
   - .knobyte/context/architecture.md (data flow, core components, boundaries)
   - .knobyte/context/conventions.md (code style, naming conventions, error handling)
4. Update .knobyte/ROUTER.md with references to each context file.
5. Update .knobyte/AGENTS.md with non-negotiable repository rules.
```

### Option B: Fresh Project

If starting a greenfield project:
1. Edit `.knobyte/context/stack.md` to define your intended technologies.
2. Record your first architecture decision:
   ```bash
   knobyte log "Adopted Rust and SQLite for local high-performance indexing" --kind decision --tags "database,architecture"
   ```

---

## 4. Indexing Code Graph & Wiki

Once your code and context files are ready, build the deterministic AST code graph and full-text search index:

```bash
# Rebuild the Tree-sitter code graph (auto-syncs to CozoDB)
knobyte graph rebuild

# Rebuild the SQLite FTS5 search index (auto-syncs to CozoDB)
knobyte wiki rebuild-index
```

---

## 5. Verifying Installation & Health

Run the diagnostic suite:

```bash
knobyte doctor
```

Output:
```
=== Knobyte Health Diagnostic ===
Overall Health: healthy
Git Repository: yes
Scaffold Root:  /path/to/project/.knobyte
Code Graph:     ready (251 nodes, 325 edges)
Wiki Index:     ready (3 entities)
CozoDB Engine:  ready (Datalog + HNSW Vector)
```

Check documentation drift against code symbols:
```bash
knobyte check
```

---

## 6. Launching the Local Project Hub

Knobyte includes a built-in web dashboard to explore your code graph, wiki, handoffs, and team state visually in your browser:

```bash
knobyte hub --port 3000
```

Open **[http://localhost:3000](http://localhost:3000)** in your browser.
