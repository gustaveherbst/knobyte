// Search: hybrid full-text + Cozo vector search over wiki and code, with
// scope / mode / type / kind filters and paging. Shows the active backend.

import { h, fill, api, pageHeader, empty, badge, link, enc, setQuery, debounce, errorBox, truncate, card, kv, fmtNum, fmtTime, navigate } from '../core.js';

const PAGE = 20;

function resultItem(it) {
    const href = it.kind === 'wiki' ? '/knowledge/' + enc(it.id) : '/code/symbols/' + enc(it.id);
    const sub = it.kind === 'wiki'
        ? [it.type, it.file].filter(Boolean).join(' · ')
        : [it.nodeKind, (it.file || '') + (it.line ? ':' + it.line : '')].filter(Boolean).join(' · ');
    return h('li', { class: 'result' },
        h('a', { href, 'data-link': '', class: 'result-link' },
            h('div', { class: 'result-head' }, badge(it.kind === 'wiki' ? 'knowledge' : 'code', it.kind === 'wiki' ? 'accent' : 'success'),
                h('span', { class: 'result-title', text: it.title || it.name || it.id })),
            h('div', { class: 'mono small muted', text: sub }),
            it.summary ? h('div', { class: 'small', text: truncate(it.summary, 220) }) : it.signature ? h('div', { class: 'mono small', text: truncate(it.signature, 220) }) : null),
        h('div', { class: 'result-why muted small' },
            (it.sources || []).map(s => badge(s === 'vector' ? 'semantic' : 'text match', s === 'vector' ? 'warning' : 'neutral')),
            it.similarity !== undefined ? h('span', { text: ' similarity ' + it.similarity }) : null));
}

export async function page(ctx) {
    const { main } = ctx;
    const q = ctx.query;
    const state = {
        q: q.get('q') || '', scope: q.get('scope') || 'all', mode: q.get('mode') || 'hybrid',
        type: q.get('type') || '', kind: q.get('kind') || '', offset: parseInt(q.get('offset') || '0', 10) || 0,
    };
    const input = h('input', { type: 'search', id: 'search-input', class: 'input search-big', value: state.q, placeholder: 'Search knowledge and code…', 'aria-label': 'Search query', autocomplete: 'off', spellcheck: 'false' });
    const results = h('div', { class: 'results', 'aria-live': 'polite' });
    const backend = h('div', { class: 'backend muted small' });
    const facets = h('div', { class: 'facets' });
    const sel = (id, label, opts, val) => h('label', { class: 'field inline' }, h('span', { class: 'field-label', text: label }),
        h('select', { class: 'input input-sm', id, onchange: ev => { state[id.replace('search-', '')] = ev.target.value; state.offset = 0; run(); } },
            opts.map(([v, l]) => h('option', { value: v, selected: v === val ? true : null, text: l }))));
    let seq = 0;

    async function run() {
        setQuery({ q: state.q || null, scope: state.scope === 'all' ? null : state.scope, mode: state.mode === 'hybrid' ? null : state.mode,
            type: state.type || null, kind: state.kind || null, offset: state.offset || null });
        if (state.q.trim().length < 2) { fill(results, empty('Type at least two characters. Press / anywhere to search.')); fill(facets); return; }
        const my = ++seq;
        fill(results, h('div', { class: 'loading', text: 'Searching…' }));
        let d;
        try {
            d = await api('/api/search/full?q=' + enc(state.q) + '&scope=' + state.scope + '&mode=' + state.mode + '&limit=' + PAGE + '&offset=' + state.offset
                + (state.type ? '&type=' + enc(state.type) : '') + (state.kind ? '&kind=' + enc(state.kind) : ''));
        } catch (err) { if (my === seq) fill(results, errorBox(err)); return; }
        if (my !== seq) return;
        const b = d.backend;
        fill(backend, 'Embeddings: ', h('strong', { text: b.embedding.backend + (b.embedding.model ? ' (' + b.embedding.model + ')' : '') }),
            ' · vector index ', badge(b.vector, b.vector === 'available' ? 'success' : b.vector === 'skipped' ? 'neutral' : 'warning'),
            b.vectorError ? h('span', { title: b.vectorError, text: ' · ' + truncate(b.vectorError, 90) }) : null,
            ' · full-text ', badge(b.fts ? 'on' : 'off', b.fts ? 'success' : 'neutral'));
        const facetChips = (title, map, key) => Object.keys(map || {}).length ? h('div', { class: 'facet' }, h('span', { class: 'muted small', text: title + ': ' }),
            Object.entries(map).map(([k, n]) => h('button', { type: 'button', class: 'chip chip-sm' + (state[key] === k ? ' active' : ''), 'aria-pressed': String(state[key] === k), text: k + ' ' + n,
                onclick: () => { state[key] = state[key] === k ? '' : k; state.offset = 0; run(); } }))) : null;
        fill(facets, facetChips('Knowledge types', d.facets.wikiTypes, 'type'), facetChips('Code kinds', d.facets.codeKinds, 'kind'));
        fill(results,
            h('div', { class: 'muted small', role: 'status', text: d.total + ' result' + (d.total === 1 ? '' : 's') + (d.total ? ' · showing ' + (d.offset + 1) + '–' + (d.offset + d.items.length) : '') }),
            (d.notes || []).map(n => h('div', { class: 'notice', text: n })),
            d.items.length ? h('ol', { class: 'result-list', start: d.offset + 1 }, d.items.map(resultItem)) : empty('No matches.'),
            h('div', { class: 'pager' },
                h('button', { type: 'button', class: 'btn btn-sm', text: '← Previous', disabled: d.offset === 0, onclick: () => { state.offset = Math.max(0, d.offset - PAGE); run(); } }),
                h('button', { type: 'button', class: 'btn btn-sm', text: 'Next →', disabled: d.nextOffset === null || d.nextOffset === undefined, onclick: () => { state.offset = d.nextOffset; run(); } })));
    }

    input.addEventListener('input', debounce(() => { state.q = input.value; state.offset = 0; run(); }, 250));
    input.addEventListener('keydown', ev => { if (ev.key === 'Enter') { state.q = input.value; state.offset = 0; run(); } });
    fill(main,
        pageHeader('Search', 'Hybrid search: full-text matches fused with semantic (vector) matches from CozoDB.'),
        h('div', { class: 'search-bar' }, input),
        h('div', { class: 'filters' },
            sel('search-scope', 'Scope', [['all', 'Knowledge & code'], ['wiki', 'Knowledge'], ['code', 'Code']], state.scope),
            sel('search-mode', 'Mode', [['hybrid', 'Hybrid'], ['fts', 'Full-text only'], ['vector', 'Semantic only']], state.mode)),
        backend, facets, results);
    input.focus();
    run();
}

/// `/code`: the code landing page — code-graph status and a code-scoped search that opens
/// symbols in the symbol workspace.
export async function codeLanding(ctx) {
    const { main } = ctx;
    const input = h('input', { type: 'search', id: 'code-search-input', class: 'input search-big', placeholder: 'Find a function, type or file…', 'aria-label': 'Search code', autocomplete: 'off', spellcheck: 'false' });
    const go = () => { const q = input.value.trim(); if (q) navigate('/search?scope=code&q=' + enc(q)); };
    input.addEventListener('keydown', ev => { if (ev.key === 'Enter') go(); });
    const status = h('div', null, h('div', { class: 'loading', text: 'Loading graph status…' }));
    fill(main,
        pageHeader('Code', 'Browse the code graph: search symbols, then open callers, callees, impact and the knowledge grounded in them.'),
        h('div', { class: 'search-bar' }, input, h('button', { type: 'button', class: 'btn btn-primary', text: 'Search code', onclick: go })),
        status);
    input.focus();
    try {
        const st = await api('/api/graph/status');
        const n = st.status || st;
        fill(status, card('Code graph', kv([
            ['Files', fmtNum(n.files !== undefined ? n.files : n.file_count)],
            ['Symbols', fmtNum(n.nodes !== undefined ? n.nodes : n.node_count)],
            ['Edges', fmtNum(n.edges !== undefined ? n.edges : n.edge_count)],
            ['Last indexed', fmtTime(n.last_indexed || n.lastIndexed)],
        ])));
    } catch (err) {
        fill(status, err.status === 404
            ? empty('The code graph is not built yet. ', link('/health', 'Open Health'), ' to build it.')
            : errorBox(err));
    }
}
