// Workstreams: list by state, detail (steps, checkpoints, relays), create
// and update through preview → apply.

import { h, fill, api, pageHeader, card, empty, badge, statusBadge, fmtTime, link, enc, kv, openDialog, field, lines, render, navigate } from '../core.js';
import { runOperation } from '../team.js';

const STATES = ['planned', 'active', 'blocked', 'done', 'archived'];
// Archiving is its own operation (workstream.archive), never a state edit.
const EDIT_STATES = STATES.filter(s => s !== 'archived');
const csv = v => String(v || '').split(',').map(x => x.trim()).filter(Boolean);

async function form(title, w) {
    w = w || {};
    return openDialog(title, close => {
        const t = h('input', { class: 'input', id: 'ws-title', maxlength: 256, value: w.title || '', autofocus: true });
        const state = h('select', { class: 'input', id: 'ws-state' }, EDIT_STATES.map(s => h('option', { value: s, selected: s === (w.status || 'planned') ? true : null, text: s })));
        const goal = h('textarea', { class: 'input', id: 'ws-goal', value: w.goal || '' });
        const summary = h('textarea', { class: 'input', id: 'ws-summary', value: w.summary || '' });
        const current = h('textarea', { class: 'input', id: 'ws-current', value: w.currentState || '' });
        const milestone = h('input', { class: 'input', id: 'ws-milestone', value: w.nextMilestone || '' });
        const owners = h('input', { class: 'input', id: 'ws-owners', value: (w.owners || []).join(', '), placeholder: 'member ids, comma separated' });
        const contributors = h('input', { class: 'input', id: 'ws-contributors', value: (w.contributors || []).join(', ') });
        const paths = h('textarea', { class: 'input', id: 'ws-paths', value: (w.paths || []).join('\n'), placeholder: 'repository paths, one per line' });
        const blockers = h('textarea', { class: 'input', id: 'ws-blockers', value: (w.blockers || []).join('\n') });
        const err = h('div', { role: 'alert' });
        return [
            h('div', { class: 'field-row' }, field('Title', t), field('State', state)),
            field('Goal', goal), field('Summary', summary),
            h('div', { class: 'field-row' }, field('Current state', current), field('Next milestone', milestone)),
            h('div', { class: 'field-row' }, field('Owners', owners), field('Contributors', contributors)),
            h('div', { class: 'field-row' }, field('Paths', paths), field('Blockers', blockers)),
            err,
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview', onclick: () => {
                    if (!t.value.trim()) { fill(err, h('div', { class: 'notice notice-error', text: 'A title is required.' })); return; }
                    close({ title: t.value.trim(), state: state.value, goal: goal.value.trim(), summary: summary.value.trim(), currentState: current.value.trim(),
                        nextMilestone: milestone.value.trim(), owners: csv(owners.value), contributors: csv(contributors.value), paths: lines(paths.value), blockers: lines(blockers.value) });
                } })),
        ];
    }, { wide: true, sticky: true });
}

async function create() {
    const v = await form('New workstream');
    if (!v) return;
    const input = { title: v.title, state: v.state, owners: v.owners, contributors: v.contributors, paths: v.paths };
    if (v.goal) input.goal = v.goal;
    if (v.summary) input.summary = v.summary;
    if (v.nextMilestone) input.nextMilestone = v.nextMilestone;
    const res = await runOperation({ kind: 'workstream.create', workstream: input }, { title: 'Create workstream', okText: 'Create' });
    if (res && res.result && res.result.id) navigate('/workstreams/' + enc(res.result.id)); else if (res) render();
}

async function update(w) {
    const v = await form('Update workstream', w);
    if (!v) return;
    const patch = {};
    const same = (a, b) => JSON.stringify(a || []) === JSON.stringify(b || []);
    if (v.title !== w.title) patch.title = v.title;
    if (v.state !== w.status) patch.state = v.state;
    for (const k of ['goal', 'summary', 'currentState', 'nextMilestone']) if ((v[k] || '') !== (w[k] || '')) patch[k] = v[k];
    for (const k of ['owners', 'contributors', 'paths', 'blockers']) if (!same(v[k], w[k])) patch[k] = v[k];
    if (await runOperation({ kind: 'workstream.update', workstreamId: w.id, patch }, { title: 'Update "' + w.title + '"' })) render();
}

async function archive(w) {
    const res = await runOperation({ kind: 'workstream.archive', workstreamId: w.id },
        { title: 'Archive "' + w.title + '"', okText: 'Archive', danger: true });
    if (res) render();
}

export async function page(ctx) {
    const { main, params } = ctx;
    if (params.id) {
        const d = await api('/api/workstreams/' + enc(params.id));
        const w = d.workstream;
        fill(main,
            link('/workstreams', '← Workstreams', 'back-link'),
            pageHeader(w.title, h('span', { class: 'detail-meta' }, statusBadge(w.status), h('span', { class: 'mono small', text: w.id }), h('span', { class: 'muted small', text: 'updated ' + fmtTime(w.updatedAt) })),
                w.status !== 'archived' ? h('button', { type: 'button', class: 'btn', text: 'Archive…', onclick: () => archive(w) }) : null,
                w.status !== 'archived' ? h('button', { type: 'button', class: 'btn btn-primary', text: 'Update', onclick: () => update(w) }) : null),
            h('div', { class: 'two-col' },
                h('div', null,
                    card('Overview', kv([['Goal', w.goal], ['Summary', w.summary || w.description], ['Current state', w.currentState], ['Next milestone', w.nextMilestone],
                        ['Owners', (w.owners || []).map(o => '@' + o).join(', ') || w.owner], ['Contributors', (w.contributors || []).map(o => '@' + o).join(', ')],
                        ['Paths', (w.paths || []).join(', ')], ['Topics', (w.topics || []).join(', ')]])),
                    card('Steps', (w.steps || []).length ? h('ol', { class: 'steps' }, w.steps.map(s => h('li', null, statusBadge(s.status), ' ', h('strong', { text: s.title }),
                        (s.filesTouched || []).length ? h('div', { class: 'muted small mono', text: s.filesTouched.join(', ') }) : null))) : empty('No steps recorded.')),
                    (w.blockers || []).length ? card('Blockers', h('ul', { class: 'plain' }, w.blockers.map(b => h('li', { text: b })))) : null),
                h('div', null,
                    card('Relays', d.relays.length ? h('ul', { class: 'plain' }, d.relays.map(r => h('li', null, link('/relays/' + enc(r.id), r.title), ' ', statusBadge(r.status)))) : empty('No relays in this workstream.')),
                    card('Checkpoints', (w.checkpoints || []).length ? h('ul', { class: 'plain' }, w.checkpoints.slice(-10).reverse().map(c => h('li', null,
                        badge(c.status), ' ', h('span', { class: 'mono small', text: c.gitHead }), h('span', { class: 'muted small', text: ' ' + fmtTime(c.timestamp) })))) : empty('No checkpoints.')))));
        return;
    }
    const state = ctx.query.get('state') || '';
    const data = await api('/api/workstreams?limit=100' + (state ? '&state=' + enc(state) : ''));
    fill(main,
        pageHeader('Workstreams', 'Longer-running efforts with owners, steps and handoffs.',
            h('button', { type: 'button', class: 'btn btn-primary', text: 'New workstream', onclick: create })),
        h('div', { class: 'chip-row', role: 'group', 'aria-label': 'State' }, [['', 'Open'], ...STATES.map(s => [s, s])].map(([v, l]) =>
            h('a', { href: '/workstreams' + (v ? '?state=' + v : ''), 'data-link': '', class: 'chip' + (state === v ? ' active' : ''), 'aria-pressed': String(state === v), text: l }))),
        data.items.length ? h('div', { class: 'table-wrap card' }, h('table', { class: 'data' },
            h('thead', null, h('tr', null, ['Workstream', 'State', 'Owners', 'Next milestone', 'Updated'].map(t => h('th', { scope: 'col', text: t })))),
            h('tbody', null, data.items.map(w => h('tr', null,
                h('td', null, link('/workstreams/' + enc(w.id), w.title)), h('td', null, statusBadge(w.status)),
                h('td', { text: (w.owners || []).map(o => '@' + o).join(', ') || (w.owner ? '@' + w.owner : '—') }),
                h('td', { text: w.nextMilestone || '—' }), h('td', { text: fmtTime(w.updatedAt) })))))) : empty('No workstreams here yet.'));
}
