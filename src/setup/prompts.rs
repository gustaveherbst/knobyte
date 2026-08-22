//! Population prompts for `knobyte setup`, one per project state:
//! - fresh: no source files yet, so the agent interviews the user;
//! - existing with a scanner brief: structure from the brief, code facts from the graph;
//! - existing without a brief: code facts from the graph, narrow high-level discovery;
//! - agent-memory: an operational workspace rather than a code repository.

use crate::setup::ProjectState;

const POPULATE_RULE: &str = "Every scaffold file you fill starts with a `<!-- knobyte:populate -->` marker comment. Remove that marker (and the \"Population pending\" note below it in AGENTS.md and ROUTER.md) from a file once it holds real content (keep both on any file you could not fill). Setup only finalizes once AGENTS.md, ROUTER.md and every context/ file is free of the marker; when they are, run `knobyte setup --finish` to capture grounding baselines and refresh the indexes. Set `last_updated` to today's date in every file you change, and keep each file's existing frontmatter keys (`id`, `title`, `type`, `summary`, `status`, `revision`).";

const GRAPH_GROUNDING_WORKFLOW: &str = r#"
CODE-GRAPH WORKFLOW (use this for all implementation understanding):

Setup has already built .knobyte/graph.db. Do not walk the source tree or open
representative implementation files to learn what code does. Use the graph from
this project root instead:

- `knobyte graph scope "<task or domain>" --json`: primary discovery tool. It returns
  bounded, source-backed facts with node ids and readable references
  (`kind:path:qualified_name`).
- `knobyte graph get <id...> --source`: read the body of nodes you intend to ground.
- `knobyte graph query where-defined <symbol>`: resolve an exact symbol.
- `knobyte graph query who-calls <symbol>` / `what-calls <symbol>`: follow calls.
- `knobyte impact <symbol|file>`: transitive blast radius when useful.

Use the pre-analysed brief only for high-level structure: folders, tooling,
dependencies and entry points. Use graph output for claims about specific code
behaviour. If the graph is unavailable, say that setup cannot author trustworthy
grounding; never invent references.

The governing rule is: READ BROAD, GROUND TIGHT.

1. Read the relevant `knobyte graph scope` neighbourhood, expanding node bodies with
   `knobyte graph get <id> --source` as needed.
2. In each scaffold file that makes a specific behavioural claim, replace its empty
   `grounds_to: []` with only the functions or methods that embody that claim, as
   references copied exactly from graph output:

   grounds_to:
     - function:src/auth.rs:validate_token

   Never ground every node scope returns: callers and callees are reading context,
   not grounding targets. Do not ground files, imports or vague components.
3. When prose names a load-bearing function, method or type you looked up, you may add
   an inline anchor on its own line: `<!-- kb-ground: kind:path:qualified_name -->`.
4. Broad architecture, stack and conventions files ground sparsely or stay
   `grounds_to: []`. Pattern files and deep domain files ground tightly to the few
   symbols that implement their documented behaviour.
"#;

const POPULATION_MERGE_RULES: &str = r#"
This may be a first population or a resumed setup over an authored scaffold.
Before editing, inventory .knobyte/AGENTS.md, .knobyte/ROUTER.md,
.knobyte/patterns/INDEX.md, every existing .knobyte/context/*.md file, and every
existing project pattern.

Treat substantive existing content as durable project knowledge:
- Fill annotation-only, placeholder and empty sections; preserve verified prose,
  routes, non-negotiables, decisions, managed blocks, custom context files and
  project-specific patterns.
- Change an authored fact only when the brief or code graph shows it is stale, with
  the smallest evidence-backed correction.
- Never delete or rename an authored context or pattern file to make the scaffold
  resemble the templates.
"#;

const EXISTING_PASS_1: &str = r#"
Populate each incomplete .knobyte/context/ section by replacing the annotation comments
with real content from this codebase. Preserve substantive content under the merge
rules above. Follow each annotation's guidance:
- Use the actual names, patterns and structures from this codebase, not generic examples.
- Do not leave a section empty: if you cannot determine the answer, write
  "[TO DETERMINE]" and explain what information is needed.
- Keep length within each annotation's guidance.
- Apply the code-graph workflow: specific behavioural claims get tight grounds_to
  entries; broad inventory or conceptual prose stays sparse or ungrounded.

Then assess whether the project has domains complex enough that cramming them into
architecture.md would make it too long or too shallow. If so, create domain context
files in .knobyte/context/ (for example context/auth.md or context/payments.md) with
the same frontmatter shape (id, title, type, summary, status, triggers, relations,
grounds_to, last_updated). Only for domains with real depth.

Then minimally update .knobyte/ROUTER.md: fill "Current Project State", add routing
rows for any domain files you created, and keep existing routes.

Update .knobyte/AGENTS.md: fill the one-line description, non-negotiables and commands;
preserve existing rules."#;

const EXISTING_PASSES_2_3: &str = r#"
PASS 2: audit and extend project patterns.

Read .knobyte/patterns/README.md for the format and categories. Audit existing
project patterns and .knobyte/patterns/INDEX.md first; preserve useful patterns and
correct only evidence-backed stale details.

If no project-specific patterns exist, write 3-5 starter patterns. If some exist,
add one only for a genuinely missing, high-value task. Prioritise:
- the 1-2 tasks a developer does most often (add an endpoint, add a command, ...)
- the 1-2 integrations with the most non-obvious gotchas
- 1 debug pattern for the most common failure boundary

Each pattern must be specific to this project: real file paths, real gotchas, real
verify steps. Run `knobyte graph scope "<pattern task>"` before writing each one and
ground it tightly. Then reconcile .knobyte/patterns/INDEX.md with the pattern files
(one row per task).

PASS 3: wire the web.

Re-read every scaffold file you audited or wrote and add or update the `relations`
list in its frontmatter. Each relation is `{type: related_to, target_id: <entity id>,
note: <when to follow it>}`, where the target is another scaffold file's frontmatter
`id` (for example kb_stack or kb_pattern_add_endpoint):
- every context/ file has at least 2 relations
- every pattern file has at least 1 relation (usually to the relevant context file)
- make relations bidirectional where it makes sense

Only write content derived from the codebase. Do not include system-injected text
(dates other than last_updated, reminders, ...) in any scaffold file.

When done, run `knobyte check` and report which files were populated and which
sections you could not fill with confidence."#;

/// Fresh project: interview the user.
pub fn build_fresh_prompt() -> String {
    format!(
        r#"You are going to populate the Knobyte project-memory scaffold for a project that is
just starting. Nothing is built yet.

Read these files in order before doing anything else:
1. .knobyte/AGENTS.md: the project operating contract
2. .knobyte/ROUTER.md: the scaffold structure
3. Every .knobyte/context/*.md file: annotations and existing content
4. .knobyte/patterns/README.md and .knobyte/patterns/INDEX.md: format and routes
5. Every existing project pattern in .knobyte/patterns/

{merge}
{populate}

Use substantive existing content to avoid repeating questions it already answers.
Then ask me the unresolved questions below one section at a time, waiting for my
answer before moving on:

1. What does this project do? (one sentence)
2. What are the hard rules: things that must never happen in this codebase?
3. What is the tech stack? (language, framework, database, key libraries)
4. Why this stack over the alternatives?
5. How will the major pieces connect? Describe the flow of a typical request or action.
6. Which patterns do you want to enforce from day one?
7. What are you deliberately NOT building or using?

After I answer, fill only incomplete .knobyte/context/ sections from my answers. For
anything still undecided write "[TO BE DETERMINED]" and note what must be decided.
Create domain context files only for domains with real depth. Update ROUTER.md's
"Current Project State" to say this is a new project and add routing rows for any new
files. Fill AGENTS.md's description, non-negotiables and commands.

Read .knobyte/patterns/README.md. If no project patterns exist, write 2-3 starter
patterns for the most obvious first tasks on this stack, marking unknowns
"[VERIFY AFTER FIRST IMPLEMENTATION]", and list them in .knobyte/patterns/INDEX.md.

Finally add `related_to` `relations` between related scaffold files by frontmatter
`id` (every context/ file at least 2, every pattern at least 1), each with a `note`
saying when to follow it.

Only write content derived from my answers. Do not include system-injected text in
any scaffold file."#,
        merge = POPULATION_MERGE_RULES.trim(),
        populate = POPULATE_RULE
    )
}

/// Existing project with the scanner brief embedded.
pub fn build_existing_with_brief_prompt(brief_json: &str) -> String {
    format!(
        r#"You are going to populate the Knobyte project-memory scaffold for this project.
The scaffold lives in the .knobyte/ directory.

Read these files in order before doing anything else:
1. .knobyte/AGENTS.md: the project operating contract
2. .knobyte/ROUTER.md: the scaffold structure
3. Every .knobyte/context/*.md file: annotations and existing content
4. .knobyte/patterns/README.md and .knobyte/patterns/INDEX.md: format and routes
5. Every existing project pattern in .knobyte/patterns/
{graph}
{merge}
{populate}

Here is a pre-analysed brief of the codebase. Do NOT explore the filesystem yourself
for basic structure: reason from this brief for dependencies, entry points, tooling
and folder layout. For implementation details use the graph workflow above rather than
opening source files.

<brief>
{brief}
</brief>

PASS 1: populate the knowledge files.
{pass1}
{pass23}"#,
        graph = GRAPH_GROUNDING_WORKFLOW,
        merge = POPULATION_MERGE_RULES.trim(),
        populate = POPULATE_RULE,
        brief = brief_json,
        pass1 = EXISTING_PASS_1,
        pass23 = EXISTING_PASSES_2_3
    )
}

/// Existing project without a brief.
pub fn build_existing_no_brief_prompt() -> String {
    format!(
        r#"You are going to populate the Knobyte project-memory scaffold for this project.
The scaffold lives in the .knobyte/ directory.

Read these files in order before doing anything else:
1. .knobyte/AGENTS.md: the project operating contract
2. .knobyte/ROUTER.md: the scaffold structure
3. Every .knobyte/context/*.md file: annotations and existing content
4. .knobyte/patterns/README.md and .knobyte/patterns/INDEX.md: format and routes
5. Every existing project pattern in .knobyte/patterns/
{graph}
{merge}
{populate}

No scanner brief is available. You may inspect manifests, README documentation and
folder names for high-level structure only (or run `knobyte init --json`). Do not
sample implementation files: use `knobyte graph scope` and the query and impact
commands for code behaviour.

PASS 1: populate the knowledge files.
{pass1}
{pass23}"#,
        graph = GRAPH_GROUNDING_WORKFLOW,
        merge = POPULATION_MERGE_RULES.trim(),
        populate = POPULATE_RULE,
        pass1 = EXISTING_PASS_1,
        pass23 = EXISTING_PASSES_2_3
    )
}

/// Persistent agent workspace.
pub fn build_agent_memory_prompt() -> String {
    format!(
        r#"You are going to populate a Knobyte scaffold for a persistent AI agent workspace.
This is not primarily a code repository: the scaffold describes an operational
environment, the agent's working memory, recurring maintenance routines, and the
patterns the agent should reuse across sessions.

Read these files first:
1. .knobyte/AGENTS.md: compact operating contract and GROW checklist
2. .knobyte/ROUTER.md: session bootstrap and routing table
3. .knobyte/HEARTBEAT.md: lightweight periodic health checks
4. .knobyte/context/*.md: the annotated context files
5. .knobyte/patterns/README.md: pattern format

{populate}

Populate the scaffold for the agent-memory use case:
- ROUTER.md: current operational state, active systems, known issues, routing table
- context/architecture.md: services, machines, containers, automations, data flows
- context/stack.md: models, tools, runtimes, storage, important versions
- context/conventions.md: naming, safety rules, operational habits
- context/decisions.md: key decisions and rationale that should not be re-litigated
- context/setup.md: how to inspect, run, restart and recover the environment
- HEARTBEAT.md: concrete checks the agent runs when polled on a heartbeat
- patterns/: 3-5 operational runbooks for recurring maintenance or debug tasks

Use the GROW loop exactly:
G (Ground): identify what changed in reality.
R (Record): update ROUTER.md and the relevant context files with current truth.
O (Orient): create or update a pattern when the task can recur.
W (Write): bump last_updated on changed scaffold files and run `knobyte log` for rationale.

Use state files for current truth and `knobyte log` for decisions, risks, todos and
notes about why something changed. Do not rewrite history out of decisions.md;
supersede old decisions instead.

Do not invent cloud services, servers, models or schedules. When something is
unknown, write [TO DETERMINE] and say what needs to be inspected."#,
        populate = POPULATE_RULE
    )
}

/// Pick the prompt for `mode` and `state`.
pub fn build_population_prompt(mode: &str, state: ProjectState, brief_json: Option<&str>) -> String {
    if mode == "agent-memory" {
        return build_agent_memory_prompt();
    }
    match (state, brief_json) {
        (ProjectState::Fresh, _) => build_fresh_prompt(),
        (_, Some(brief)) => build_existing_with_brief_prompt(brief),
        (_, None) => build_existing_no_brief_prompt(),
    }
}
