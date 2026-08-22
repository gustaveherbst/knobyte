// Catch up: what changed in shared memory since the working member last caught
// up, grouped (handoffs, reviews, decisions, knowledge, workstreams, playbooks,
// activity). "Mark caught up" moves only this checkout's local cursor, through
// the same preview → apply protocol as every other team write.

import { h, fill, api, pageHeader, card, empty, badge, fmtTime, link, enc, render } from '../core.js';
import { runOperation } from '../team.js';

const GROUPS = [
    ['handoffs', 'Handoffs for you'],
    ['reviews', 'Proposals awaiting your review'],
    ['decisions', 'Decisions'],
    ['knowledge', 'Knowledge changes'],
    ['workstreams', 'Workstreams'],
    ['playbooks', 'Playbooks'],
    ['activity', 'Other activity'],
];

const WINDOWS = [['', 'Since last catch-up'], ['1d', 'Last day'], ['7d', 'Last week'], ['30d', 'Last month']];

function hrefFor(item) {
    const subj = (item.subjects || [])[0] || {};
    switch (item.source) {
        case 'relay': return '/relays/' + enc(subj.id);
        case 'proposal': return '/inbox/' + enc(subj.id);
        case 'wiki': return subj.id ? '/knowledge/' + enc(subj.id) : null;
        default: break;
    }
    if (subj.kind === 'workstream') return '/workstreams/' + enc(subj.id);
    if (subj.kind === 'playbook') return '/playbooks/' + enc(subj.id);
    if (subj.kind === 'playbook_run') return '/playbooks/runs/' + enc(subj.id);
    if (subj.kind === 'relay') return '/relays/' + enc(subj.id);
    return null;
}

function itemRow(item) {
    const href = hrefFor(item);
    return h('li', { class: 'catchup-item' },
        h('div', null,
            href ? link(href, item.title) : h('strong', { text: item.title }),
            item.new ? null : ' ', item.new ? null : badge('still open', 'warning')),
        h('div', { class: 'muted small', text: item.summary }),
        h('div', { class: 'muted small' }, fmtTime(item.occurredAt), item.actor ? ' · @' + item.actor : '', item.workstream ? ' · ' + item.workstream : ''));
}

async function markCaughtUp(d) {
    const res = await runOperation({ kind: 'catchup.mark', at: d.observedAt }, { title: 'Mark caught up', okText: 'Mark caught up', quick: true });
    if (res) render();
}

async function resetCursor() {
    const res = await runOperation({ kind: 'catchup.reset' }, { title: 'Reset the catch-up cursor to now (this branch)', okText: 'Reset' });
    if (res) render();
}

export async function page(ctx) {
    const { main } = ctx;
    const since = ctx.query.get('since') || '';
    const mine = ctx.query.get('mine') === '1';
    const d = await api('/api/catch-up?limit=100' + (since ? '&since=' + enc(since) : '') + (mine ? '&includeMine=true' : ''));
    const href = (s, m) => {
        const q = new URLSearchParams();
        if (s) q.set('since', s);
        if (m) q.set('mine', '1');
        const t = q.toString();
        return '/catch-up' + (t ? '?' + t : '');
    };
    const items = d.items || [];
    const branchChanged = (d.diagnostics || []).some(x => x.code === 'CATCH_UP_BRANCH_CHANGED');
    const baselineText = d.baselineSource === 'cursor' ? 'since you last caught up (' + fmtTime(d.baseline) + ')'
        : d.baselineSource === 'since' ? 'since ' + fmtTime(d.baseline)
            : 'in the last 7 days (no catch-up recorded yet)';
    fill(main,
        pageHeader('Catch up', 'What changed ' + baselineText + ', for ' + (d.actorId || 'unknown') + '.',
            d.cursor ? h('button', { type: 'button', class: 'btn', text: 'Reset…', onclick: resetCursor }) : null,
            h('button', { type: 'button', class: 'btn btn-primary', text: 'Mark caught up', disabled: branchChanged || d.actorId === 'unknown' ? true : null, onclick: () => markCaughtUp(d) })),
        h('div', { class: 'chip-row', role: 'group', 'aria-label': 'Window' }, WINDOWS.map(([v, l]) =>
            h('a', { href: href(v, mine), 'data-link': '', class: 'chip' + (since === v ? ' active' : ''), 'aria-pressed': String(since === v), text: l })),
            h('a', { href: href(since, !mine), 'data-link': '', class: 'chip' + (mine ? ' active' : ''), 'aria-pressed': String(mine), text: 'Include my changes' })),
        (d.diagnostics || []).map(x => h('div', { class: 'notice notice-warning', role: 'status', text: x.message })),
        items.length ? GROUPS.map(([g, label]) => {
            const inGroup = items.filter(i => i.group === g);
            if (!inGroup.length) return null;
            return card(label + ' (' + inGroup.length + ')', h('ul', { class: 'plain catchup-list' }, inGroup.map(itemRow)));
        }) : empty('You are all caught up.'),
        d.truncated ? h('p', { class: 'muted small', text: 'Showing the first ' + items.length + ' of ' + d.total + ' items.' }) : null,
        h('p', { class: 'muted small' }, 'The cursor is stored in this checkout only (.knobyte/local), never committed. ', link('/activity', 'Full activity history')));
}
