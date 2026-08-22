// Setup wizard: needs_git → needs_setup → needs_population → needs_finalize
// → needs_commit → ready. Population launches an agent only after explicit
// confirmation and streams its transcript; the commit is reviewed file by
// file and confirmed by the user.

import { h, fill, api, post, pageHeader, card, empty, badge, statusBadge, link, enc, toast, confirmDialog, stream, errorBox, unifiedDiff, field, refreshShell, kv } from '../core.js';

const STAGES = [
    ['needs_git', 'Git repository'], ['needs_setup', 'Scaffold & tools'], ['needs_population', 'Populate'],
    ['needs_finalize', 'Finalize'], ['needs_commit', 'Commit'], ['ready', 'Ready'],
];

function stepper(stage) {
    const idx = Math.max(0, STAGES.findIndex(([s]) => s === (stage === 'complete' ? 'ready' : stage)));
    return h('ol', { class: 'stepper', 'aria-label': 'Setup progress' }, STAGES.map(([s, l], i) => h('li', {
        class: 'step' + (i < idx ? ' done' : '') + (i === idx ? ' current' : ''), 'aria-current': i === idx ? 'step' : null,
    }, h('span', { class: 'step-n', 'aria-hidden': 'true', text: i < idx ? '✓' : String(i + 1) }), h('span', { text: l }))));
}

function runBox(run) {
    if (!run || run.status === 'idle') return null;
    return h('div', { class: 'notice ' + (run.status === 'failed' ? 'notice-error' : run.status === 'running' ? 'notice-info' : run.status === 'succeeded' ? 'notice-success' : 'notice-warning'), role: 'status' },
        statusBadge(run.status), ' ', h('strong', { text: run.message }),
        run.error ? h('div', { class: 'small', text: run.error }) : null,
        (run.anchorNotes || []).length ? h('ul', { class: 'plain small' }, run.anchorNotes.map(n => h('li', { text: n }))) : null);
}

const sleep = ms => new Promise(r => setTimeout(r, ms));

/// Resolve with the setup run once it is no longer running. Progress arrives
/// over SSE (/api/setup/events); polling /api/setup/run every second is the
/// fallback when the stream cannot be opened or is closed by the server.
export function waitForRun(onTick) {
    return new Promise(resolve => {
        let settled = false;
        let close = null;
        let polling = false;
        const finish = run => { if (settled) return; settled = true; if (close) close(); resolve(run); };
        const tick = run => { if (settled) return; if (onTick) onTick(run); if (run.status !== 'running') finish(run); };
        const poll = async () => {
            if (polling) return;
            polling = true;
            if (close) close();
            while (!settled) {
                try { tick(await api('/api/setup/run')); } catch (_) { /* transient */ }
                if (!settled) await sleep(1000);
            }
        };
        if (typeof EventSource !== 'function') { poll(); return; }
        close = stream('/api/setup/events', {
            run: tick,
            error: es => { if (es.readyState === 2) poll(); },
        });
    });
}

export async function page(ctx) {
    const { main } = ctx;
    const st = await api('/api/setup');
    const stops = [];
    ctx.cleanup(() => stops.forEach(f => f()));
    const reload = async () => { await refreshShell(); if (ctx.isCurrent()) page(ctx); };
    const body = h('div', { class: 'setup-body' });
    fill(main,
        pageHeader('Setup', h('span', null, 'Set up Knobyte project memory for ', h('strong', { text: st.projectName }), ' ', h('span', { class: 'mono small muted', text: st.projectRoot }))),
        stepper(st.stage),
        runBox(st.run),
        body);
    const stage = st.stage;
    if (st.run && st.run.status === 'running' && st.run.action !== 'population') {
        fill(body, card('Working…', h('div', { class: 'loading', text: st.run.message })));
        await waitForRun();
        return reload();
    }
    if (stage === 'needs_git') return gitStep(body, reload);
    if (stage === 'needs_setup') return setupStep(body, st, reload);
    if (stage === 'needs_population') return populationStep(body, st, reload, stops);
    if (stage === 'needs_finalize') return finalizeStep(body, reload);
    if (stage === 'needs_commit') return commitStep(body, st, reload);
    fill(body, card('Project memory is ready', [
        h('p', { text: stage === 'complete' ? 'This agent-memory workspace is set up.' : 'The scaffold is populated, finalized and committed. Agents read .knobyte/ through the anchors in your AI tool files.' }),
        h('ul', { class: 'plain' },
            h('li', null, link('/', 'Open the overview')), h('li', null, link('/context', 'Explore the context graph')),
            h('li', null, link('/health', 'Check context health')), h('li', null, link('/members', 'Add your team'))),
    ]));
}

function gitStep(body, reload) {
    fill(body, card('Initialize a git repository', [
        h('p', { text: 'Knobyte keeps shared project memory in git: drift history, relays and reviews travel with commits. This folder has no git repository yet.' }),
        h('div', { class: 'action-row' },
            h('button', { type: 'button', class: 'btn btn-primary', text: 'Run git init…', onclick: async () => {
                if (!await confirmDialog('Initialize git?', 'Run `git init` in this project folder? Nothing is committed.', { okText: 'Initialize' })) return;
                try { await post('/api/setup/git-init', { confirm: true }); toast('Git repository initialized', 'success'); reload(); } catch (err) { toast(err.message, 'error'); }
            } }),
            h('span', { class: 'muted small', text: 'Or switch the mode to agent-memory on the next step if this is not a code repository.' })),
    ]));
}

function setupStep(body, st, reload) {
    const mode = h('select', { class: 'input', id: 'setup-mode' }, ['code-repo', 'agent-memory'].map(m => h('option', { value: m, selected: m === st.mode ? true : null, text: m })));
    const preselect = st.configuredTools.length ? st.configuredTools : ['claude'];
    const tools = h('fieldset', { class: 'tool-grid' }, h('legend', { class: 'field-label', text: 'AI tools to configure' }),
        st.tools.map(t => h('label', { class: 'tool-choice', for: 'tool-' + t.id },
            h('input', { type: 'checkbox', id: 'tool-' + t.id, value: t.id, checked: preselect.includes(t.id) }),
            h('span', { class: 'tool-name', text: t.name }),
            t.launchable ? badge(t.cliAvailable ? 'CLI found' : 'CLI not found', t.cliAvailable ? 'success' : 'neutral') : null)));
    const skipGraph = h('input', { type: 'checkbox', id: 'setup-skip-graph' });
    const err = h('div', { role: 'alert' });
    fill(body, card('Create the scaffold', [
        h('p', { text: 'Creates .knobyte/, links your AI tools to it, installs agent skills, scans the codebase and builds the code graph. No agent is launched in this step.' }),
        field('Mode', mode, 'code-repo for a codebase; agent-memory for a persistent agent workspace.'),
        tools,
        h('label', { class: 'check', for: 'setup-skip-graph' }, skipGraph, ' Skip building the code graph (build it later from Health)'),
        err,
        h('div', { class: 'action-row' }, h('button', { type: 'button', class: 'btn btn-primary', text: 'Run setup', onclick: async ev => {
            ev.currentTarget.disabled = true;
            const selected = Array.from(tools.querySelectorAll('input:checked')).map(i => i.value);
            try {
                await post('/api/setup', { mode: mode.value, tools: selected, skipGraph: skipGraph.checked });
                fill(body, card('Setting up…', h('div', { class: 'loading', role: 'status', text: 'Creating the scaffold, tool anchors, skills and code graph…' })));
                const run = await waitForRun();
                if (run.status === 'failed') toast(run.error || run.message, 'error'); else toast(run.message, 'success');
                reload();
            } catch (e) { fill(err, errorBox(e)); ev.target.disabled = false; }
        } })),
    ]));
}

function transcriptView(runId, stops, onDone) {
    const list = h('ol', { class: 'transcript', 'aria-live': 'polite', 'aria-label': 'Agent transcript' });
    const status = h('div', { class: 'muted small', role: 'status', text: 'Connecting…' });
    let last = 0;
    const add = e => {
        if (e.id <= last) return;
        last = e.id;
        list.appendChild(h('li', { class: 'tx tx-' + e.kind }, h('span', { class: 'tx-kind', text: e.kind }), h('span', { class: 'tx-text', text: e.text })));
        list.scrollTop = list.scrollHeight;
        status.textContent = last + ' entr' + (last === 1 ? 'y' : 'ies');
    };
    const close = stream('/api/setup/transcript/events?run=' + enc(runId), {
        entry: add,
        done: () => { close(); status.textContent = 'Session ended.'; onDone(); },
        error: () => { status.textContent = 'Stream interrupted; reconnecting…'; },
    });
    stops.push(close);
    return h('div', { class: 'transcript-box' }, status, list);
}

async function populationStep(body, st, reload, stops) {
    const run = st.run || {};
    if (run.status === 'running' && run.action === 'population' && run.transcriptId) {
        fill(body, card('Populating with ' + (run.populationTool || 'agent'), [
            h('p', { class: 'muted', text: 'The agent reads your codebase and fills every scaffold file. You can cancel at any time; the process tree is stopped.' }),
            transcriptView(run.transcriptId, stops, () => setTimeout(reload, 400)),
            h('div', { class: 'action-row' }, h('button', { type: 'button', class: 'btn btn-danger', text: 'Cancel session', onclick: async ev => {
                ev.currentTarget.disabled = true;
                try { await post('/api/setup/cancel', {}); toast('Cancelling the agent session…'); } catch (err) { toast(err.message, 'error'); }
            } })),
        ]));
        return;
    }
    let pv;
    try { pv = await post('/api/setup/population/preview', {}); } catch (err) { fill(body, errorBox(err)); return; }
    const toolSel = h('select', { class: 'input', id: 'pop-tool' }, pv.installedAgents.map(t => h('option', { value: t, selected: t === pv.tool ? true : null, text: t === 'claude' ? 'Claude Code' : 'Codex' })));
    const copyBtn = h('button', { type: 'button', class: 'btn btn-sm', text: 'Copy prompt', onclick: async () => {
        try { await navigator.clipboard.writeText(pv.prompt); toast('Prompt copied', 'success'); } catch (_) { toast('Copy failed; select the text instead', 'error'); }
    } });
    fill(body,
        card('Populate the scaffold', [
            h('p', { text: 'These files still carry the populate marker: ' + (st.unpopulatedFiles || []).join(', ') }),
            pv.installedAgents.length ? [
                field('Agent', toolSel),
                h('p', { class: 'muted small', text: 'Knobyte will run this command in ' + pv.cwd + ' (timeout ' + pv.timeoutMinutes + ' min). The full prompt is written to a private file that is removed afterwards.' }),
                h('pre', { class: 'signature', text: pv.command || '' }),
                h('div', { class: 'action-row' }, h('button', { type: 'button', class: 'btn btn-primary', text: 'Launch agent…', onclick: async () => {
                    const tool = toolSel.value;
                    const ok = await confirmDialog('Launch ' + (tool === 'claude' ? 'Claude Code' : 'Codex') + '?',
                        h('div', null, h('p', { class: 'dialog-text', text: 'The agent will read this repository and edit files under .knobyte/ (accept-edits mode, read-only Knobyte commands allowed). It may use your AI provider account.' }),
                            h('pre', { class: 'signature', text: pv.command || '' })), { okText: 'Launch' });
                    if (!ok) return;
                    try { await post('/api/setup/population', { tool, confirm: true }); reload(); } catch (err) { toast(err.message, 'error'); }
                } })),
            ] : h('div', { class: 'notice notice-warning', text: 'Neither the Claude Code nor the Codex CLI is installed on PATH. Paste the prompt below into your AI tool instead.' }),
        ]),
        card('Or paste the prompt yourself', [
            h('p', { class: 'muted', text: 'Paste into any AI tool that can read and edit files in this repository; when it finishes, check again.' }),
            h('div', { class: 'action-row' }, copyBtn, h('button', { type: 'button', class: 'btn btn-sm', text: 'Check again', onclick: reload })),
            h('details', null, h('summary', { text: 'Show prompt (' + pv.promptChars + ' characters)' }), h('pre', { class: 'body', text: pv.prompt })),
        ]));
}

function finalizeStep(body, reload) {
    fill(body, card('Finalize', [
        h('p', { text: 'Capture grounding baselines for the populated scaffold and build the wiki index.' }),
        h('div', { class: 'action-row' }, h('button', { type: 'button', class: 'btn btn-primary', text: 'Finalize', onclick: async ev => {
            ev.currentTarget.disabled = true;
            try { const r = await post('/api/setup/finalize', {}); toast(r.run.message, 'success'); } catch (err) { toast(err.message, 'error'); }
            reload();
        } })),
    ]));
}

async function commitStep(body, st, reload) {
    let review;
    try { review = await post('/api/setup/commit/preview', {}); } catch (err) { fill(body, errorBox(err)); return; }
    const diffBox = h('div', { class: 'diff-box', 'aria-live': 'polite' }, empty('Select a file to view its diff.'));
    const msg = h('textarea', { class: 'input', id: 'commit-msg', value: review.defaultMessage, maxlength: 2000 });
    const files = h('ul', { class: 'commit-files' }, review.files.map(f => h('li', null, h('button', { type: 'button', class: 'commit-file', onclick: async ev => {
        files.querySelectorAll('.commit-file').forEach(b => b.classList.toggle('selected', b === ev.currentTarget));
        fill(diffBox, h('div', { class: 'loading', text: 'Loading diff…' }));
        try {
            const d = await post('/api/setup/commit/diff', { revision: review.revision, path: f.path });
            fill(diffBox, h('div', { class: 'mono small', text: d.path }), unifiedDiff(d.diff), d.truncated ? h('div', { class: 'muted small', text: 'Diff truncated.' }) : null);
        } catch (err) { fill(diffBox, errorBox(err)); }
    } }, badge(f.status, f.status === 'added' ? 'success' : f.status === 'deleted' ? 'danger' : 'warning'), h('span', { class: 'mono grow', text: f.path }),
        h('span', { class: 'diffstat' }, h('span', { class: 'add', text: '+' + f.additions }), ' ', h('span', { class: 'del', text: '−' + f.deletions }))))));
    fill(body, card('Review and commit', [
        h('p', { text: 'Knobyte stages and commits exactly these files (nothing is pushed). Review each diff, then confirm.' }),
        kv([['Branch', review.branch || 'detached'], ['HEAD', h('span', { class: 'mono small', text: review.head || '(no commits yet)' })], ['Files', String(review.files.length)]]),
        review.blockedReason ? h('div', { class: 'notice notice-warning', text: review.blockedReason }) : null,
        h('div', { class: 'commit-review' }, files, diffBox),
        field('Commit message', msg),
        h('div', { class: 'action-row' },
            h('button', { type: 'button', class: 'btn btn-primary', text: 'Commit ' + review.files.length + ' file(s)…', disabled: !review.canCommit, onclick: async () => {
                if (!await confirmDialog('Create this commit?', 'Stage and commit the ' + review.files.length + ' reviewed file(s) with your message? Nothing is pushed.', { okText: 'Commit' })) return;
                try {
                    const r = await post('/api/setup/commit', { revision: review.revision, message: msg.value });
                    toast('Committed ' + r.commit.substring(0, 10), 'success');
                    reload();
                } catch (err) {
                    toast(err.message, 'error');
                    if (err.status === 409) reload();
                }
            } }),
            h('button', { type: 'button', class: 'btn', text: 'Review again', onclick: reload })),
        h('details', null, h('summary', { text: 'Prefer the terminal?' }), h('pre', { class: 'body', text: st.commitCommands.join('\n') })),
    ]));
}
