// Playbooks: list by state, detail (steps, runs), create / update / publish /
// archive, and runs (start, complete a step with evidence, abandon). Every
// write is a preview → apply of the exact signed envelope.

import { h, fill, api, pageHeader, card, empty, badge, statusBadge, fmtTime, link, enc, kv, openDialog, field, lines, render, navigate } from '../core.js';
import { runOperation, parseEvidence, evidenceText } from '../team.js';

const STATES = ['draft', 'active', 'archived'];
const RUN_STATES = ['active', 'completed', 'abandoned'];
const csv = v => String(v || '').split(',').map(x => x.trim()).filter(Boolean);

/// Steps editor text: one step per block, "Title" on the first line, then
/// optional "> description" lines and "evidence: ..." lines.
function stepsToText(steps) {
    return (steps || []).map(s => [s.title]
        .concat(s.description ? s.description.split('\n').map(l => '> ' + l) : [])
        .concat((s.requiredChecks || []).map(c => 'check: ' + c))
        .concat((s.expectedEvidence || []).map(e => 'evidence: ' + e)).join('\n')).join('\n\n');
}

function textToSteps(text, previous) {
    const blocks = String(text || '').split(/\n\s*\n/).map(b => b.split('\n').map(l => l.trim()).filter(Boolean)).filter(b => b.length);
    return blocks.map(b => {
        const step = { title: b[0] };
        const prev = (previous || []).find(p => p.title === b[0]);
        if (prev) step.id = prev.id;
        const desc = b.slice(1).filter(l => l.startsWith('>')).map(l => l.slice(1).trim());
        const checks = b.slice(1).filter(l => /^check:/i.test(l)).map(l => l.slice(6).trim()).filter(Boolean);
        const evidence = b.slice(1).filter(l => /^evidence:/i.test(l)).map(l => l.slice(9).trim()).filter(Boolean);
        if (desc.length) step.description = desc.join('\n');
        if (checks.length) step.requiredChecks = checks;
        if (evidence.length) step.expectedEvidence = evidence;
        return step;
    });
}

async function form(title, p) {
    p = p || {};
    return openDialog(title, close => {
        const t = h('input', { class: 'input', id: 'pb-title', maxlength: 512, value: p.title || '', autofocus: true });
        const state = h('select', { class: 'input', id: 'pb-state' }, ['draft', 'active'].map(s => h('option', { value: s, selected: s === (p.state || 'draft') ? true : null, text: s })));
        const summary = h('textarea', { class: 'input', id: 'pb-summary', value: p.summary || '' });
        const trigger = h('input', { class: 'input', id: 'pb-trigger', value: p.trigger || '', placeholder: 'When to use this playbook' });
        const owners = h('input', { class: 'input', id: 'pb-owners', value: (p.owners || []).join(', '), placeholder: 'member ids, comma separated (default: you)' });
        const topics = h('input', { class: 'input', id: 'pb-topics', value: (p.topics || []).join(', ') });
        const prereq = h('textarea', { class: 'input', id: 'pb-prereq', value: (p.prerequisites || []).join('\n'), placeholder: 'one per line' });
        const steps = h('textarea', { class: 'input mono', id: 'pb-steps', rows: 10, value: stepsToText(p.steps),
            placeholder: 'Run the test suite\n> cargo test --all\ncheck: no failing tests\nevidence: test output\n\nTag the release\nevidence: commit' });
        const err = h('div', { role: 'alert' });
        return [
            h('div', { class: 'field-row' }, field('Title', t), field('State', state, 'Draft playbooks cannot be run yet.')),
            field('Summary', summary), field('Trigger', trigger),
            h('div', { class: 'field-row' }, field('Owners', owners), field('Topics', topics)),
            field('Prerequisites', prereq),
            field('Steps', steps, 'One block per step, separated by a blank line: title, then "> description", "check: …" and "evidence: …" lines.'),
            err,
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview', onclick: () => {
                    if (!t.value.trim()) { fill(err, h('div', { class: 'notice notice-error', text: 'A title is required.' })); return; }
                    const parsed = textToSteps(steps.value, p.steps);
                    if (state.value === 'active' && !parsed.length) { fill(err, h('div', { class: 'notice notice-error', text: 'An active playbook needs at least one step.' })); return; }
                    close({ title: t.value.trim(), state: state.value, summary: summary.value.trim(), trigger: trigger.value.trim(),
                        owners: csv(owners.value), topics: csv(topics.value), prerequisites: lines(prereq.value), steps: parsed });
                } })),
        ];
    }, { wide: true, sticky: true });
}

async function create() {
    const v = await form('New playbook');
    if (!v) return;
    const input = { title: v.title, state: v.state, owners: v.owners, topics: v.topics, prerequisites: v.prerequisites, steps: v.steps };
    if (v.summary) input.summary = v.summary;
    if (v.trigger) input.trigger = v.trigger;
    const res = await runOperation({ kind: 'playbook.create', playbook: input }, { title: 'Create playbook', okText: 'Create' });
    if (res && res.result && res.result.id) navigate('/playbooks/' + enc(res.result.id)); else if (res) render();
}

async function update(p) {
    const v = await form('Update playbook', p);
    if (!v) return;
    const patch = {};
    const same = (a, b) => JSON.stringify(a || []) === JSON.stringify(b || []);
    if (v.title !== p.title) patch.title = v.title;
    if (v.state !== p.state) patch.state = v.state;
    for (const k of ['summary', 'trigger']) if ((v[k] || '') !== (p[k] || '')) patch[k] = v[k];
    for (const k of ['owners', 'topics', 'prerequisites']) if (!same(v[k], p[k])) patch[k] = v[k];
    const norm = steps => (steps || []).map(s => ({ id: s.id, title: s.title, description: s.description || undefined,
        requiredChecks: (s.requiredChecks || []).length ? s.requiredChecks : undefined, expectedEvidence: (s.expectedEvidence || []).length ? s.expectedEvidence : undefined }));
    if (JSON.stringify(norm(v.steps)) !== JSON.stringify(norm(p.steps))) patch.steps = v.steps;
    if (!Object.keys(patch).length) return;
    if (await runOperation({ kind: 'playbook.update', playbookId: p.id, patch }, { title: 'Update "' + p.title + '"' })) render();
}

async function publish(p) {
    if (await runOperation({ kind: 'playbook.update', playbookId: p.id, patch: { state: 'active' } }, { title: 'Publish "' + p.title + '"', okText: 'Publish' })) render();
}

async function archive(p) {
    if (await runOperation({ kind: 'playbook.archive', playbookId: p.id }, { title: 'Archive "' + p.title + '"', okText: 'Archive', danger: true })) render();
}

async function startRun(p) {
    let workstreams = [];
    try { workstreams = (await api('/api/workstreams?limit=100')).items || []; } catch (_) { /* optional */ }
    const v = await openDialog('Start "' + p.title + '"', close => {
        const label = h('input', { class: 'input', id: 'run-title', maxlength: 512, placeholder: 'Optional label, e.g. release 1.4' , autofocus: true });
        const ws = h('select', { class: 'input', id: 'run-ws' }, [h('option', { value: '', text: 'No workstream' })]
            .concat(workstreams.map(w => h('option', { value: w.id, text: w.title + ' (' + w.id + ')' }))));
        return [
            h('p', { class: 'dialog-text', text: 'The run takes a snapshot of the playbook\'s ' + p.steps.length + ' step(s); later playbook edits do not change it.' }),
            field('Label', label), field('Workstream', ws),
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview', onclick: () => close({ title: label.value.trim(), workstream: ws.value }) })),
        ];
    });
    if (!v) return;
    const action = { kind: 'playbook.run.start', playbookId: p.id };
    if (v.title) action.title = v.title;
    if (v.workstream) action.workstream = v.workstream;
    const res = await runOperation(action, { title: 'Start run', okText: 'Start' });
    if (res && res.result && res.result.id) navigate('/playbooks/runs/' + enc(res.result.id));
}

async function completeStep(run, step) {
    const v = await openDialog('Complete "' + step.title + '"', close => {
        const evidence = h('textarea', { class: 'input', id: 'step-evidence', rows: 4, autofocus: true,
            placeholder: 'one per line: file:<path>, commit:<sha>, entity:<id>, a URL, or free text' });
        const note = h('textarea', { class: 'input', id: 'step-note', rows: 2 });
        return [
            step.description ? h('p', { class: 'dialog-text', text: step.description }) : null,
            (step.requiredChecks || []).length ? h('div', { class: 'muted small' }, 'Checks: ', step.requiredChecks.join(' · ')) : null,
            (step.expectedEvidence || []).length ? h('div', { class: 'muted small' }, 'Expected evidence: ', step.expectedEvidence.join(' · ')) : null,
            field('Evidence', evidence), field('Note', note),
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview', onclick: () => close({ evidence: lines(evidence.value).map(parseEvidence), note: note.value.trim() }) })),
        ];
    }, { wide: true });
    if (!v) return;
    const action = { kind: 'playbook.run.complete-step', runId: run.id, stepId: step.stepId, evidence: v.evidence };
    if (v.note) action.note = v.note;
    if (await runOperation(action, { title: 'Complete step', okText: 'Complete' })) render();
}

async function abandon(run) {
    const reason = await openDialog('Abandon this run', close => {
        const r = h('textarea', { class: 'input', id: 'run-reason', rows: 3, autofocus: true });
        const err = h('div', { role: 'alert' });
        return [
            field('Reason', r), err,
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-danger', text: 'Preview', onclick: () => {
                    if (!r.value.trim()) { fill(err, h('div', { class: 'notice notice-error', text: 'A reason is required.' })); return; }
                    close(r.value.trim());
                } })),
        ];
    });
    if (!reason) return;
    if (await runOperation({ kind: 'playbook.run.abandon', runId: run.id, reason }, { title: 'Abandon run', okText: 'Abandon', danger: true })) render();
}

function runRows(runs) {
    return h('div', { class: 'table-wrap card' }, h('table', { class: 'data' },
        h('thead', null, h('tr', null, ['Run', 'Playbook', 'State', 'Progress', 'Started'].map(t => h('th', { scope: 'col', text: t })))),
        h('tbody', null, runs.map(r => h('tr', null,
            h('td', null, link('/playbooks/runs/' + enc(r.id), r.title || r.id)),
            h('td', null, link('/playbooks/' + enc(r.playbookId), r.playbookTitle)),
            h('td', null, statusBadge(r.state)),
            h('td', { text: r.stepsCompleted + ' / ' + r.stepsTotal }),
            h('td', { text: fmtTime(r.startedAt) + ' · @' + r.startedBy }))))));
}

async function runPage(main, id) {
    const run = await api('/api/playbook-runs/' + enc(id));
    const done = run.steps.filter(s => s.state === 'completed').length;
    const nextPending = run.steps.find(s => s.state !== 'completed');
    fill(main,
        link('/playbooks/' + enc(run.playbookId), '← ' + run.playbookTitle, 'back-link'),
        pageHeader(run.title || run.playbookTitle,
            h('span', { class: 'detail-meta' }, statusBadge(run.state), h('span', { class: 'mono small', text: run.id }),
                h('span', { class: 'muted small', text: done + ' of ' + run.steps.length + ' steps · started ' + fmtTime(run.startedAt) + ' by @' + run.startedBy })),
            run.state === 'active' ? h('button', { type: 'button', class: 'btn', text: 'Abandon…', onclick: () => abandon(run) }) : null),
        h('div', { class: 'two-col' },
            h('div', null,
                card('Steps', h('ol', { class: 'steps' }, run.steps.map(s => h('li', null,
                    statusBadge(s.state), ' ', h('strong', { text: s.title }),
                    s.description ? h('div', { class: 'muted small', text: s.description }) : null,
                    s.state === 'completed'
                        ? h('div', { class: 'small' }, 'Done by @' + s.completedBy + ' · ' + fmtTime(s.completedAt),
                            (s.evidence || []).length ? h('ul', { class: 'plain small' }, s.evidence.map(e => h('li', { class: 'mono', text: evidenceText(e) }))) : null,
                            s.note ? h('div', { class: 'muted small', text: s.note }) : null)
                        : h('div', null,
                            (s.expectedEvidence || []).length ? h('div', { class: 'muted small', text: 'Expected evidence: ' + s.expectedEvidence.join(' · ') }) : null,
                            run.state === 'active' ? h('button', { type: 'button', class: 'btn btn-small' + (s === nextPending ? ' btn-primary' : ''), text: 'Complete…', onclick: () => completeStep(run, s) }) : null)))))),
            h('div', null,
                card('Run', kv([['Playbook', link('/playbooks/' + enc(run.playbookId), run.playbookTitle)], ['Playbook revision', run.playbookRevision],
                    ['Workstream', run.workstream ? link('/workstreams/' + enc(run.workstream), run.workstream) : null],
                    ['Started at commit', run.startedAtCommit], ['Completed', run.completedAt ? fmtTime(run.completedAt) + ' by @' + run.completedBy : null],
                    run.abandonReason ? ['Abandoned', fmtTime(run.abandonedAt) + ' by @' + run.abandonedBy + ': ' + run.abandonReason] : null])))));
}

export async function page(ctx) {
    const { main, params } = ctx;
    if (params.run) return runPage(main, params.run);
    if (params.id) {
        const d = await api('/api/playbooks/' + enc(params.id));
        const p = d.playbook;
        const editable = p.state !== 'archived';
        fill(main,
            link('/playbooks', '← Playbooks', 'back-link'),
            pageHeader(p.title, h('span', { class: 'detail-meta' }, statusBadge(p.state), h('span', { class: 'mono small', text: p.id }),
                h('span', { class: 'muted small', text: 'revision ' + p.entityRevision + ' · updated ' + fmtTime(p.updatedAt) + ' by @' + p.updatedBy })),
                editable ? h('button', { type: 'button', class: 'btn', text: 'Archive…', onclick: () => archive(p) }) : null,
                editable ? h('button', { type: 'button', class: 'btn', text: 'Edit', onclick: () => update(p) }) : null,
                p.state === 'draft' ? h('button', { type: 'button', class: 'btn btn-primary', text: 'Publish', onclick: () => publish(p) }) : null,
                p.state === 'active' ? h('button', { type: 'button', class: 'btn btn-primary', text: 'Start run', onclick: () => startRun(p) }) : null),
            h('div', { class: 'two-col' },
                h('div', null,
                    card('Overview', kv([['Summary', p.summary], ['Trigger', p.trigger], ['Owners', (p.owners || []).map(o => '@' + o).join(', ')],
                        ['Topics', (p.topics || []).join(', ')], ['Prerequisites', (p.prerequisites || []).join('; ')]])),
                    card('Steps', (p.steps || []).length ? h('ol', { class: 'steps' }, p.steps.map(s => h('li', null, h('strong', { text: s.title }),
                        s.description ? h('div', { class: 'muted small', text: s.description }) : null,
                        (s.requiredChecks || []).length ? h('div', { class: 'small' }, badge('checks', 'neutral'), ' ', s.requiredChecks.join(' · ')) : null,
                        (s.expectedEvidence || []).length ? h('div', { class: 'small' }, badge('evidence', 'accent'), ' ', s.expectedEvidence.join(' · ')) : null)))
                        : empty('No steps yet. Edit the playbook to add some.'))),
                h('div', null, card('Runs', d.runs.length ? h('ul', { class: 'plain' }, d.runs.map(r => h('li', null,
                    link('/playbooks/runs/' + enc(r.id), r.title || r.id), ' ', statusBadge(r.state),
                    h('span', { class: 'muted small', text: ' ' + r.stepsCompleted + '/' + r.stepsTotal + ' · ' + fmtTime(r.startedAt) })))) : empty('Not run yet.')))));
        return;
    }
    const view = ctx.query.get('view') || '';
    const state = ctx.query.get('state') || '';
    const chips = view === 'runs'
        ? [['', 'All runs'], ...RUN_STATES.map(s => [s, s])].map(([v, l]) =>
            h('a', { href: '/playbooks?view=runs' + (v ? '&state=' + v : ''), 'data-link': '', class: 'chip' + (state === v ? ' active' : ''), 'aria-pressed': String(state === v), text: l }))
        : [['', 'Current'], ...STATES.map(s => [s, s])].map(([v, l]) =>
            h('a', { href: '/playbooks' + (v ? '?state=' + v : ''), 'data-link': '', class: 'chip' + (state === v ? ' active' : ''), 'aria-pressed': String(state === v), text: l }));
    const tabs = h('div', { class: 'chip-row', role: 'group', 'aria-label': 'View' },
        h('a', { href: '/playbooks', 'data-link': '', class: 'chip' + (view !== 'runs' ? ' active' : ''), 'aria-pressed': String(view !== 'runs'), text: 'Playbooks' }),
        h('a', { href: '/playbooks?view=runs', 'data-link': '', class: 'chip' + (view === 'runs' ? ' active' : ''), 'aria-pressed': String(view === 'runs'), text: 'Runs' }));
    const header = pageHeader('Playbooks', 'Reusable step-by-step procedures, and the runs that record each step\'s evidence.',
        h('button', { type: 'button', class: 'btn btn-primary', text: 'New playbook', onclick: create }));
    if (view === 'runs') {
        const data = await api('/api/playbook-runs?limit=100' + (state ? '&state=' + enc(state) : ''));
        fill(main, header, tabs, h('div', { class: 'chip-row', role: 'group', 'aria-label': 'State' }, chips),
            data.items.length ? runRows(data.items) : empty('No runs here yet.'));
        return;
    }
    const data = await api('/api/playbooks?limit=100' + (state ? '&state=' + enc(state) : ''));
    fill(main, header, tabs, h('div', { class: 'chip-row', role: 'group', 'aria-label': 'State' }, chips),
        data.items.length ? h('div', { class: 'table-wrap card' }, h('table', { class: 'data' },
            h('thead', null, h('tr', null, ['Playbook', 'State', 'Steps', 'Owners', 'Updated'].map(t => h('th', { scope: 'col', text: t })))),
            h('tbody', null, data.items.map(p => h('tr', null,
                h('td', null, link('/playbooks/' + enc(p.id), p.title), p.summary ? h('div', { class: 'muted small', text: p.summary }) : null),
                h('td', null, statusBadge(p.state)),
                h('td', { text: String((p.steps || []).length) }),
                h('td', { text: (p.owners || []).map(o => '@' + o).join(', ') || '—' }),
                h('td', { text: fmtTime(p.updatedAt) })))))) : empty('No playbooks here yet.'));
}
