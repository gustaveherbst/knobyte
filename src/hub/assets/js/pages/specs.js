// Specs (/specs, /specs/:id): list filtered by lifecycle, and a detail view with the spec's
// hierarchy (requirements, acceptance criteria, constraints), groundings and grounding
// rollup, plus "propose an update".

import { h, fill, api, pageHeader, empty, badge, statusBadge, link, enc, card, navigate } from '../core.js';
import { editDraft } from './inbox.js';

const LIFECYCLE = [['', 'Active'], ['in_flight', 'In flight'], ['promoted', 'Promoted'], ['deprecated', 'Deprecated'], ['archived', 'Archived']];
const HEALTH_KIND = { fresh: 'success', unverified: 'neutral', ambiguous: 'warning', changed: 'warning', missing: 'danger' };

function healthBadge(health) { return badge(health || 'unverified', HEALTH_KIND[health] || 'neutral'); }

function itemList(items, empty_text) {
    return items.length ? h('ul', { class: 'plain' }, items.map(it => h('li', null,
        link('/knowledge/' + enc(it.id), it.title), ' ', statusBadge(it.lifecycleState || it.status), ' ', healthBadge(it.groundingHealth))))
        : h('div', { class: 'muted small', text: empty_text });
}

function hierarchy(sp) {
    const hy = sp.hierarchy || {};
    return card('Hierarchy', h('div', null,
        h('h3', { class: 'small-title', text: 'Requirements' }), itemList(hy.requirements || [], 'No requirements.'),
        h('h3', { class: 'small-title', text: 'Acceptance criteria' }), itemList(hy.acceptanceCriteria || [], 'No acceptance criteria.'),
        h('h3', { class: 'small-title', text: 'Constraints' }), itemList(hy.constraints || [], 'No constraints.'),
        (hy.relations || []).length ? h('details', null, h('summary', { text: (hy.relations || []).length + ' relation(s)' }),
            h('ul', { class: 'plain mono small' }, hy.relations.map(r => h('li', { text: r.source + ' —' + r.type + '→ ' + r.target })))) : null));
}

function groundings(sp) {
    const rollup = Object.entries(sp.groundingRollup || {});
    return card('Groundings', h('div', null,
        rollup.length ? h('div', { class: 'chip-row', 'aria-label': 'Grounding rollup' }, rollup.map(([k, n]) => h('span', null, healthBadge(k), ' ', h('span', { class: 'small', text: String(n) })))) : null,
        (sp.groundings || []).length ? h('ul', { class: 'plain' }, sp.groundings.map(g => h('li', null,
            healthBadge(g.health), ' ', g.resolved ? link('/code/symbols/' + enc(g.resolved), g.reference, 'mono small') : h('span', { class: 'mono small', text: g.reference }))))
            : h('div', { class: 'muted small', text: 'This spec is not grounded in code.' })));
}

export async function page(ctx) {
    const { main, params } = ctx;
    const lifecycle = ctx.query.get('lifecycleStates') || '';
    const d = await api('/api/specs' + (lifecycle ? '?lifecycleStates=' + enc(lifecycle) : ''));
    const chips = h('div', { class: 'chip-row', role: 'group', 'aria-label': 'Lifecycle' }, LIFECYCLE.map(([v, l]) => h('button', {
        type: 'button', class: 'chip' + (lifecycle === v ? ' active' : ''), 'aria-pressed': String(lifecycle === v), text: l,
        onclick: () => navigate('/specs' + (params.id ? '/' + enc(params.id) : '') + (v ? '?lifecycleStates=' + enc(v) : '')),
    })));
    const list = h('div', { class: 'card list-panel' }, d.specs.length ? d.specs.map(sp => h('a', {
        href: '/specs/' + enc(sp.id) + (lifecycle ? '?lifecycleStates=' + enc(lifecycle) : ''), 'data-link': '', class: 'list-item' + (sp.id === params.id ? ' selected' : ''), 'aria-current': sp.id === params.id ? 'true' : null,
    }, h('div', { class: 'list-item-title', text: sp.title }), h('div', { class: 'list-item-meta' }, statusBadge(sp.lifecycleState || sp.status), healthBadge(sp.groundingHealth), h('span', { class: 'mono', text: sp.file }))))
        : empty(lifecycle ? 'No specs in this lifecycle state.' : 'No specs yet. Propose one with a spec.create draft.'));
    const detail = h('div', { class: 'card detail-panel' });
    if (params.id) {
        const sp = await api('/api/specs/' + enc(params.id));
        fill(detail,
            h('h2', { class: 'detail-title', text: sp.title }),
            h('div', { class: 'detail-meta' }, statusBadge(sp.lifecycleState || sp.status), healthBadge(sp.groundingHealth), h('span', { class: 'mono small', text: sp.file }), h('span', { class: 'mono small', text: sp.id })),
            sp.summary ? h('p', { text: sp.summary }) : null,
            h('div', { class: 'action-row' },
                link('/knowledge/' + enc(sp.id), 'Open knowledge view', 'btn'),
                h('button', { type: 'button', class: 'btn btn-primary', text: 'Propose an update', onclick: () => editDraft(null, {
                    change: { kind: 'spec.update', target: { id: sp.id, title: sp.title }, patch: { title: sp.title, summary: sp.summary || '', body: sp.body || '' } },
                }) })),
            hierarchy(sp),
            groundings(sp),
            h('pre', { class: 'body', text: sp.body }));
    } else fill(detail, empty('Select a spec.'));
    fill(main,
        pageHeader('Specs', 'Specifications under .knobyte/specs/.',
            h('button', { type: 'button', class: 'btn btn-primary', text: 'New spec draft', onclick: () => editDraft(null, { change: { kind: 'spec.create', entityKind: 'spec' } }) })),
        chips,
        h('div', { class: 'split' }, list, detail));
}
