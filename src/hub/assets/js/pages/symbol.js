// Symbol page (/code/symbols/:id): overview with paged source ("load
// more"), callers, callees and impact tabs, related knowledge.

import { h, fill, api, pageHeader, card, empty, badge, link, enc, kv, codeLines, errorBox } from '../core.js';

const TABS = [['overview', 'Overview'], ['callers', 'Callers'], ['callees', 'Callees'], ['impact', 'Impact']];

function nodeRow(n) {
    return h('li', { class: 'rel-row' },
        n.unresolved ? h('span', { class: 'mono', text: n.id }) : link('/code/symbols/' + enc(n.id), n.qualifiedName || n.name),
        n.kind ? badge(n.kind, 'neutral') : null,
        h('span', { class: 'mono small muted', text: (n.file || '') + (n.startLine ? ':' + n.startLine : '') }),
        n.via ? h('span', { class: 'muted small', text: ' via ' + n.via + (n.depth ? ' · depth ' + n.depth : '') + (n.callLine ? ' · line ' + n.callLine : '') }) : null);
}

async function relationList(id, which) {
    const box = h('div');
    const list = h('ul', { class: 'plain rel-list' });
    const more = h('button', { type: 'button', class: 'btn btn-sm', text: 'Load more' });
    const status = h('div', { class: 'muted small', role: 'status' });
    let offset = 0;
    const load = async () => {
        more.disabled = true;
        try {
            const d = await api('/api/code/symbol/' + which + '?id=' + enc(id) + '&offset=' + offset + '&limit=25');
            d.items.forEach(n => list.appendChild(nodeRow(n)));
            offset = d.nextOffset || offset;
            status.textContent = list.childElementCount + ' of ' + d.total;
            more.hidden = d.nextOffset === null || d.nextOffset === undefined;
            if (!d.total) fill(box, empty(which === 'callers' ? 'No recorded callers.' : 'No recorded callees.'));
        } catch (err) { fill(box, errorBox(err)); }
        more.disabled = false;
    };
    more.addEventListener('click', load);
    fill(box, status, list, more);
    await load();
    return box;
}

async function impactView(id) {
    const d = await api('/api/code/symbol/impact?id=' + enc(id) + '&depth=3');
    return [
        h('p', { class: 'muted', text: d.total + ' dependent symbol(s) within 3 hops' + (d.truncated ? ' (bounded; truncated)' : '') + '.' }),
        d.items.length ? h('ul', { class: 'plain rel-list' }, d.items.map(nodeRow)) : empty('Nothing depends on this symbol.'),
        d.groundings.length ? h('div', { class: 'detail-section' }, h('h3', { class: 'label', text: 'Scaffold documents grounded here or downstream' }),
            h('ul', { class: 'plain' }, d.groundings.map(g => h('li', { class: 'mono small', text: g.doc + (g.node_id ? ' → ' + g.node_id : '') })))) : null,
    ];
}

export async function page(ctx) {
    const { main, params } = ctx;
    const id = params.id;
    const tab = ctx.query.get('tab') || 'overview';
    const d = await api('/api/code/symbol?id=' + enc(id));
    const n = d.node;
    const src = d.source;
    const code = h('div', { class: 'source' });
    const moreBtn = h('button', { type: 'button', class: 'btn btn-block', text: 'Load more source' });
    let next = src.nextLine;
    if (src.available) {
        code.appendChild(codeLines(src.lines, n.startLine));
        moreBtn.hidden = !next;
        moreBtn.addEventListener('click', async () => {
            moreBtn.disabled = true;
            try {
                const page = await api('/api/code/symbol/source?id=' + enc(id) + '&from=' + next + '&limit=120');
                const pre = code.querySelector('pre');
                codeLines(page.lines, next).childNodes.forEach(c => pre.appendChild(c.cloneNode(true)));
                next = page.nextLine;
                moreBtn.hidden = !next;
            } catch (err) { code.appendChild(errorBox(err)); }
            moreBtn.disabled = false;
        });
    } else {
        code.appendChild(empty('Source is not available in this checkout.'));
        moreBtn.hidden = true;
    }
    const tabs = h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Symbol views' }, TABS.map(([k, l]) => h('a', {
        href: '/code/symbols/' + enc(id) + (k === 'overview' ? '' : '?tab=' + k), 'data-link': '', role: 'tab', 'aria-selected': String(tab === k), class: 'tab' + (tab === k ? ' active' : ''),
        text: l + (k === 'callers' ? ' (' + d.callers.total + ')' : k === 'callees' ? ' (' + d.callees.total + ')' : ''),
    })));
    const panel = h('div', { class: 'tab-panel', role: 'tabpanel' });
    fill(main,
        link('/search', '← Back to search', 'back-link'),
        pageHeader(n.name, h('span', { class: 'mono', text: n.qualifiedName })),
        tabs, panel);
    if (tab === 'overview') {
        fill(panel, h('div', { class: 'symbol-grid' },
            h('div', null,
                card(n.language + ' · ' + n.kind, [
                    n.signature ? h('pre', { class: 'signature', text: n.signature }) : null,
                    n.docstring ? h('p', { class: 'docstring', text: n.docstring }) : null,
                    kv([['File', h('span', { class: 'mono', text: n.file })], ['Range', n.startLine + '–' + n.endLine], ['Visibility', n.visibility],
                        ['Exported', n.isExported ? 'yes' : 'no'], ['Container', d.container ? link('/code/symbols/' + enc(d.container.id), d.container.qualifiedName || d.container.name) : '—'],
                        ['Symbol id', h('span', { class: 'mono small', text: n.id })]]),
                ]),
                card('Related knowledge', d.knowledge.length ? h('ul', { class: 'plain' }, d.knowledge.map(e => h('li', null, link('/knowledge/' + enc(e.id), e.title), h('span', { class: 'muted small', text: ' · ' + e.type })))) : empty('No knowledge entity grounds this symbol.'),
                    { aside: badge(String(d.knowledge.length), 'neutral') })),
            card('Source', [code, moreBtn], { aside: src.available ? h('span', { class: 'muted small', text: src.totalLines + ' lines' }) : null }),
            card('Relations', h('ul', { class: 'plain nav-cards' },
                h('li', null, link('/code/symbols/' + enc(id) + '?tab=callers', '← Callers: ' + d.callers.total + ' inbound reference(s)')),
                h('li', null, link('/code/symbols/' + enc(id) + '?tab=callees', '→ Callees: ' + d.callees.total + ' outbound reference(s)')),
                h('li', null, link('/code/symbols/' + enc(id) + '?tab=impact', '⇉ Impact: bounded dependents'))))));
    } else if (tab === 'impact') {
        fill(panel, card('Impact', await impactView(id)));
    } else {
        fill(panel, card(tab === 'callers' ? 'Callers' : 'Callees', await relationList(id, tab)));
    }
}
