// Activity feed: decisions, discoveries and team activity, paged and filtered on the
// server (kind, since), each entry with its context. The first page refreshes live.

import { h, fill, api, pageHeader, empty, badge, fmtTime, link, enc, setQuery } from '../core.js';

const FILTERS = [['all', 'All'], ['decision', 'Decisions'], ['discovery', 'Discoveries'], ['risk', 'Risks'], ['team', 'Team']];
const SINCE = [['', 'Any time'], ['1d', 'Last day'], ['7d', 'Last week'], ['30d', 'Last month']];
const PAGE = 50;
const ENTITY_ROUTES = { relay: '/relays/', workstream: '/workstreams/', member: '/members/', proposal: '/inbox/', inbox: '/inbox/', spec: '/specs/', entity: '/knowledge/', wiki: '/knowledge/' };

function kindBadge(kind) {
    const k = String(kind || '').toLowerCase();
    return badge(kind || 'event', k === 'decision' ? 'accent' : k === 'discovery' ? 'warning' : k === 'risk' ? 'danger' : k.startsWith('team:') ? 'success' : 'neutral');
}

function entityLink(kind, id, title) {
    const base = ENTITY_ROUTES[String(kind || '').toLowerCase()];
    if (!id) return null;
    return base ? link(base + enc(id), title || id) : h('span', { class: 'mono small', text: (kind ? kind + ':' : '') + id });
}

/// Per-entry context: the entity it concerns, workstream, subjects, files and origin.
function context(it) {
    const c = it.context || {};
    const parts = [];
    if (c.entityId) parts.push(h('span', { class: 'small' }, h('span', { class: 'muted', text: (c.entityKind || 'entity') + ' ' }), entityLink(c.entityKind, c.entityId, c.entityTitle)));
    if (c.workstream) parts.push(h('span', { class: 'small' }, h('span', { class: 'muted', text: 'workstream ' }), link('/workstreams/' + enc(c.workstream), c.workstream)));
    for (const sbj of (c.subjects || []).slice(0, 4)) {
        if (sbj.kind === 'entity') parts.push(h('span', { class: 'small' }, entityLink(sbj.entityKind, sbj.id, sbj.title)));
        else if (sbj.kind === 'code' && sbj.symbolId) parts.push(link('/code/symbols/' + enc(sbj.symbolId), sbj.symbolId, 'mono small'));
        else if (sbj.path) parts.push(h('span', { class: 'file-pill', text: sbj.path }));
    }
    if (c.origin && c.origin.kind) parts.push(badge(c.origin.kind === 'workflow' ? 'via ' + (c.origin.operation || 'workflow') : 'recorded', 'neutral'));
    if (c.repoState && (c.repoState.branch || c.repoState.headCommit)) parts.push(h('span', { class: 'mono small muted', text: [c.repoState.branch, String(c.repoState.headCommit || '').slice(0, 8)].filter(Boolean).join(' @ ') }));
    const files = (c.files || it.files || []);
    if (files.length) parts.push(h('div', { class: 'file-tags' }, files.slice(0, 6).map(f => h('span', { class: 'file-pill', text: f }))));
    return parts.length ? h('div', { class: 'feed-context' }, parts) : null;
}

export async function page(ctx) {
    const { main } = ctx;
    const state = { kind: ctx.query.get('kind') || 'all', since: ctx.query.get('since') || '', offset: parseInt(ctx.query.get('offset') || '0', 10) || 0 };
    const feed = h('ol', { class: 'feed', 'aria-live': 'polite', 'aria-label': 'Activity' });
    const status = h('span', { class: 'muted small', role: 'status' });
    const chips = h('div', { class: 'chip-row', role: 'group', 'aria-label': 'Filter' });
    const pager = h('div', { class: 'pager' });
    const seen = new Set();
    let first = true;
    const sinceSel = h('select', { class: 'input input-sm', id: 'activity-since', 'aria-label': 'Since', onchange: ev => { state.since = ev.target.value; state.offset = 0; load(); } },
        SINCE.map(([v, l]) => h('option', { value: v, selected: v === state.since ? true : null, text: l })));
    const drawChips = () => fill(chips, FILTERS.map(([v, l]) => h('button', { type: 'button', class: 'chip' + (state.kind === v ? ' active' : ''), 'aria-pressed': String(state.kind === v), text: l,
        onclick: () => { state.kind = v; state.offset = 0; drawChips(); load(); } })));
    async function load() {
        setQuery({ kind: state.kind === 'all' ? null : state.kind, since: state.since || null, offset: state.offset || null });
        let d;
        try {
            d = await api('/api/feed?limit=' + PAGE + '&offset=' + state.offset + '&kind=' + enc(state.kind) + (state.since ? '&since=' + enc(state.since) : ''));
        } catch (err) { status.textContent = 'Feed offline: ' + err.message; return; }
        const items = d.items.map(it => { const fresh = !first && state.offset === 0 && !seen.has(it.id); seen.add(it.id); return Object.assign(it, { fresh }); });
        first = false;
        if (!items.length) fill(feed, h('li', null, empty('No recorded activity here. Agents and `knobyte log` record decisions and discoveries.')));
        else fill(feed, items.map(it => h('li', { class: 'feed-row' + (it.fresh ? ' fresh' : '') },
            h('div', { class: 'feed-actor' }, h('strong', { text: '@' + (it.actor || 'unattributed') }), kindBadge(it.kind)),
            h('div', { class: 'feed-body' }, h('div', { text: it.summary }), it.details ? h('div', { class: 'muted small', text: it.details }) : null, context(it)),
            h('time', { class: 'feed-time muted small', datetime: it.timestamp, text: fmtTime(it.timestamp) }))));
        fill(pager,
            h('span', { class: 'muted small', text: d.total ? (d.offset + 1) + '–' + (d.offset + d.items.length) + ' of ' + d.total : '' }),
            h('button', { type: 'button', class: 'btn btn-sm', text: '← Newer', disabled: state.offset === 0, onclick: () => { state.offset = Math.max(0, state.offset - PAGE); load(); } }),
            h('button', { type: 'button', class: 'btn btn-sm', text: 'Older →', disabled: d.nextOffset === null || d.nextOffset === undefined, onclick: () => { state.offset = d.nextOffset; load(); } }));
        status.textContent = (state.offset === 0 ? 'Live · ' : 'Paused on an older page · ') + 'updated ' + new Date().toLocaleTimeString([], { hour12: false });
    }
    drawChips();
    fill(main, pageHeader('Activity', 'Decisions, discoveries and team activity. The newest page refreshes automatically.', status),
        h('div', { class: 'filters' }, chips, h('label', { class: 'field inline' }, h('span', { class: 'field-label', text: 'Since' }), sinceSel)), feed, pager);
    await load();
    const timer = setInterval(() => { if (!document.hidden && state.offset === 0) load(); }, 5000);
    ctx.cleanup(() => clearInterval(timer));
}
