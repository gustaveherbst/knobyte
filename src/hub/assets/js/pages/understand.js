// Context graph explorer: wiki entities, their relations and code groundings
// (deterministic force layout, pan / zoom / drag, keyboard selectable).

import { h, s, fill, api, pageHeader, empty, badge, statusBadge, link, enc, truncate, errorBox, reducedMotion } from '../core.js';

function layout(nodes, edges) {
    const n = nodes.length;
    if (!n) return;
    const golden = Math.PI * (3 - Math.sqrt(5));
    nodes.forEach((nd, i) => { const r = 40 * Math.sqrt(i + 1); nd.x = r * Math.cos(i * golden); nd.y = r * Math.sin(i * golden); });
    const area = Math.max(n, 4) * 9000;
    const kOpt = Math.sqrt(area / Math.max(n, 1));
    const iters = n > 400 ? 60 : n > 150 ? 140 : 260;
    let temp = kOpt * 2;
    for (let it = 0; it < iters; it++) {
        for (const v of nodes) { v.vx = 0; v.vy = 0; }
        for (let i = 0; i < n; i++) {
            const a = nodes[i];
            for (let j = i + 1; j < n; j++) {
                const b = nodes[j];
                let dx = a.x - b.x, dy = a.y - b.y;
                let d2 = dx * dx + dy * dy;
                if (d2 < 0.01) { dx = 0.1 * (i - j); dy = 0.1; d2 = dx * dx + dy * dy; }
                const f = (kOpt * kOpt) / d2;
                a.vx += dx * f; a.vy += dy * f; b.vx -= dx * f; b.vy -= dy * f;
            }
        }
        for (const e of edges) {
            const dx = e.a.x - e.b.x, dy = e.a.y - e.b.y;
            const d = Math.sqrt(dx * dx + dy * dy) || 0.01;
            const ideal = e.kind === 'grounds_to' ? kOpt * 0.6 : kOpt;
            const f = (d - ideal) / d * 0.5;
            e.a.vx -= dx * f; e.a.vy -= dy * f; e.b.vx += dx * f; e.b.vy += dy * f;
        }
        for (const v of nodes) {
            v.vx -= v.x * 0.04; v.vy -= v.y * 0.04;
            const d = Math.sqrt(v.vx * v.vx + v.vy * v.vy) || 0.01;
            const step = Math.min(d, temp);
            v.x += (v.vx / d) * step; v.y += (v.vy / d) * step;
        }
        temp *= 0.97;
    }
}

export async function page(ctx) {
    const { main } = ctx;
    const data = await api('/api/graph/context');
    const nodes = data.nodes.map((n, i) => Object.assign({}, n, { idx: i, x: 0, y: 0, vx: 0, vy: 0, deg: 0 }));
    const byId = new Map(nodes.map(n => [n.id, n]));
    const edges = [];
    for (const e of data.edges) {
        const a = byId.get(e.source), b = byId.get(e.target);
        if (!a || !b || a === b) continue;
        a.deg++; b.deg++;
        edges.push({ a, b, kind: e.kind });
    }
    layout(nodes, edges);
    const g = { nodes, edges, byId, view: { x: 0, y: 0, k: 1 }, selected: null };

    const filter = h('input', { type: 'search', class: 'input input-sm', placeholder: 'Highlight nodes…', 'aria-label': 'Highlight graph nodes' });
    const showCode = h('input', { type: 'checkbox', checked: true, id: 'graph-show-code' });
    const svgEl = s('svg', { class: 'graph-svg', role: 'application', 'aria-label': 'Context graph. Tab to a node and press Enter to inspect it.' });
    const wrap = h('div', { class: 'graph-wrap' }, svgEl);
    const detail = h('div', { class: 'card detail-panel', 'aria-live': 'polite' }, empty('Select an entity or code node in the graph.'));

    fill(main,
        pageHeader('Context', 'Wiki entities and their relationships, grounded in code. Select a node to inspect it; open its page for the full view.'),
        h('div', { class: 'split split-graph' },
            h('div', { class: 'card graph-panel' },
                h('div', { class: 'toolbar' }, filter, h('label', { class: 'check', for: 'graph-show-code' }, showCode, ' Code nodes'),
                    h('button', { type: 'button', class: 'btn btn-sm', text: 'Reset view', onclick: () => { fit(); applyView(); } })),
                h('div', { class: 'legend-row muted small' },
                    h('span', null, h('span', { class: 'legend-dot entity' }), 'Entity'),
                    h('span', null, h('span', { class: 'legend-dot code' }), 'Code node'),
                    h('span', null, h('span', { class: 'legend-dot missing' }), 'Unresolved grounding')),
                data.truncated ? h('div', { class: 'notice notice-warning', text: 'Showing the first 500 entities.' }) : null,
                nodes.length ? wrap : empty(data.hint || 'No wiki entities indexed yet. Refresh the wiki index from the Health page.')),
            detail));
    if (!nodes.length) return;

    const viewport = s('g');
    const edgeLayer = s('g'), nodeLayer = s('g');
    viewport.append(edgeLayer, nodeLayer);
    svgEl.appendChild(viewport);
    for (const e of edges) {
        e.el = s('line', { class: 'g-edge' + (e.kind === 'grounds_to' ? ' grounds' : '') }, s('title', { text: e.kind }));
        edgeLayer.appendChild(e.el);
    }
    for (const nd of nodes) {
        const r = nd.group === 'entity' ? 7 + Math.min(nd.deg, 10) * 0.6 : 5.5;
        const shape = nd.group === 'code' ? s('rect', { class: 'shape', x: -r, y: -r, width: r * 2, height: r * 2, rx: 2 }) : s('circle', { class: 'shape', r });
        const title = nd.group === 'entity' ? nd.label + ' (' + nd.entityType + ')' : nd.group === 'code' ? nd.label + ' — ' + nd.file : 'Unresolved grounding: ' + nd.nodeId;
        nd.el = s('g', { class: 'g-node ' + nd.group, tabindex: 0, role: 'button', 'aria-label': title },
            s('title', { text: title }), shape, s('text', { x: r + 4, y: 3.5, text: truncate(nd.label, 26) }));
        nd.el.addEventListener('pointerdown', ev => startNodeDrag(ev, nd));
        nd.el.addEventListener('keydown', ev => { if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); select(nd); } });
        nodeLayer.appendChild(nd.el);
    }

    function position() {
        for (const nd of nodes) nd.el.setAttribute('transform', 'translate(' + nd.x.toFixed(1) + ',' + nd.y.toFixed(1) + ')');
        for (const e of edges) {
            e.el.setAttribute('x1', e.a.x.toFixed(1)); e.el.setAttribute('y1', e.a.y.toFixed(1));
            e.el.setAttribute('x2', e.b.x.toFixed(1)); e.el.setAttribute('y2', e.b.y.toFixed(1));
        }
    }
    function fit() {
        const w = wrap.clientWidth || 800, hgt = wrap.clientHeight || 500;
        let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
        for (const nd of nodes) { minX = Math.min(minX, nd.x); minY = Math.min(minY, nd.y); maxX = Math.max(maxX, nd.x); maxY = Math.max(maxY, nd.y); }
        const gw = Math.max(maxX - minX, 1) + 160, gh = Math.max(maxY - minY, 1) + 80;
        const k = Math.max(0.15, Math.min(2, Math.min(w / gw, hgt / gh)));
        g.view = { k, x: w / 2 - ((minX + maxX) / 2) * k, y: hgt / 2 - ((minY + maxY) / 2) * k };
    }
    function applyView() { viewport.setAttribute('transform', 'translate(' + g.view.x + ',' + g.view.y + ') scale(' + g.view.k + ')'); }
    function applyCode(show) {
        for (const nd of nodes) if (nd.group !== 'entity') nd.el.style.display = show ? '' : 'none';
        for (const e of edges) e.el.style.display = (!show && (e.a.group !== 'entity' || e.b.group !== 'entity')) ? 'none' : '';
    }
    function applyFilter() {
        const q = filter.value.trim().toLowerCase();
        if (!q) { if (!g.selected) { nodes.forEach(n => n.el.classList.remove('dim')); edges.forEach(e => e.el.classList.remove('dim')); } return; }
        const match = nd => String(nd.label).toLowerCase().includes(q) || String(nd.file || '').toLowerCase().includes(q) || String(nd.entityId || nd.nodeId || '').toLowerCase().includes(q);
        nodes.forEach(n => n.el.classList.toggle('dim', !match(n)));
        edges.forEach(e => e.el.classList.toggle('dim', !(match(e.a) && match(e.b))));
    }
    function highlight(nd) {
        const neigh = new Set([nd]);
        for (const e of edges) { const on = e.a === nd || e.b === nd; e.el.classList.toggle('hl', on); if (on) { neigh.add(e.a); neigh.add(e.b); } }
        nodes.forEach(n => { n.el.classList.toggle('selected', n === nd); n.el.classList.toggle('dim', !neigh.has(n)); });
        edges.forEach(e => e.el.classList.toggle('dim', !(neigh.has(e.a) && neigh.has(e.b))));
    }
    function select(nd) {
        g.selected = nd;
        highlight(nd);
        if (nd.group === 'entity') showEntity(nd.entityId);
        else if (nd.group === 'code') showCode_(nd.resolvedId || nd.nodeId);
        else fill(detail, h('h2', { class: 'detail-title', text: 'Unresolved grounding' }), h('div', { class: 'mono small', text: nd.nodeId }),
            h('p', { text: 'A wiki entity declares this anchor, but it no longer matches a node in the code graph.' }),
            link('/groundings', 'Preview relocations', 'btn btn-primary'));
    }
    async function showEntity(id) {
        fill(detail, empty('Loading…'));
        try {
            const d = await api('/api/wiki/entity?id=' + enc(id));
            const e = d.entity;
            fill(detail,
                h('h2', { class: 'detail-title', text: e.title }),
                h('div', { class: 'detail-meta' }, badge(e.entity_type, 'accent'), statusBadge(e.status), h('span', { class: 'mono small', text: e.file })),
                e.summary ? h('p', { text: e.summary }) : null,
                h('div', { class: 'muted small', text: d.groundings.length + ' grounding(s) · ' + e.relations.length + ' relation(s) · ' + d.backlinks.length + ' backlink(s)' }),
                h('div', { class: 'action-row' }, link('/knowledge/' + enc(e.id), 'Open knowledge page →', 'btn btn-primary')));
        } catch (err) { fill(detail, errorBox(err)); }
    }
    async function showCode_(id) {
        fill(detail, empty('Loading…'));
        try {
            const d = await api('/api/code/node?id=' + enc(id));
            const n = d.node;
            fill(detail,
                h('h2', { class: 'detail-title', text: n.qualified_name || n.name }),
                h('div', { class: 'detail-meta' }, badge(n.kind, 'success'), h('span', { class: 'mono small', text: n.file_path + ':' + n.start_line })),
                n.signature ? h('pre', { class: 'signature', text: n.signature }) : null,
                h('div', { class: 'muted small', text: d.callers.length + ' caller(s) · explained by ' + d.entities.length + ' entit' + (d.entities.length === 1 ? 'y' : 'ies') }),
                h('div', { class: 'action-row' }, link('/code/symbols/' + enc(n.id), 'Open symbol page →', 'btn btn-primary')));
        } catch (err) { fill(detail, errorBox(err)); }
    }

    let drag = null;
    const pt = ev => { const rect = svgEl.getBoundingClientRect(); return { x: ev.clientX - rect.left, y: ev.clientY - rect.top }; };
    function startNodeDrag(ev, nd) { ev.stopPropagation(); drag = { type: 'node', nd, start: pt(ev), moved: false }; svgEl.setPointerCapture(ev.pointerId); }
    svgEl.addEventListener('pointerdown', ev => {
        if (drag) return;
        drag = { type: 'pan', start: pt(ev), view: Object.assign({}, g.view), moved: false };
        svgEl.classList.add('panning');
        svgEl.setPointerCapture(ev.pointerId);
    });
    svgEl.addEventListener('pointermove', ev => {
        if (!drag) return;
        const p = pt(ev);
        const dx = p.x - drag.start.x, dy = p.y - drag.start.y;
        if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
        if (drag.type === 'pan') { g.view.x = drag.view.x + dx; g.view.y = drag.view.y + dy; applyView(); }
        else if (drag.moved) { drag.nd.x = (p.x - g.view.x) / g.view.k; drag.nd.y = (p.y - g.view.y) / g.view.k; position(); }
    });
    const end = () => {
        if (!drag) return;
        if (drag.type === 'node' && !drag.moved) select(drag.nd);
        if (drag.type === 'pan' && !drag.moved && g.selected) {
            g.selected = null;
            nodes.forEach(n => n.el.classList.remove('selected', 'dim'));
            edges.forEach(e => e.el.classList.remove('hl', 'dim'));
            applyFilter();
        }
        drag = null;
        svgEl.classList.remove('panning');
    };
    svgEl.addEventListener('pointerup', end);
    svgEl.addEventListener('pointercancel', end);
    svgEl.addEventListener('wheel', ev => {
        ev.preventDefault();
        const p = pt(ev);
        const k = Math.max(0.1, Math.min(4, g.view.k * Math.exp(-ev.deltaY * 0.0015)));
        g.view.x = p.x - (p.x - g.view.x) * (k / g.view.k);
        g.view.y = p.y - (p.y - g.view.y) * (k / g.view.k);
        g.view.k = k;
        applyView();
    }, { passive: false });
    svgEl.addEventListener('keydown', ev => {
        const step = 40;
        const keys = { ArrowLeft: [step, 0], ArrowRight: [-step, 0], ArrowUp: [0, step], ArrowDown: [0, -step] };
        if (keys[ev.key] && ev.target === svgEl) { ev.preventDefault(); g.view.x += keys[ev.key][0]; g.view.y += keys[ev.key][1]; applyView(); }
        if ((ev.key === '+' || ev.key === '=') && ev.target === svgEl) { g.view.k = Math.min(4, g.view.k * 1.2); applyView(); }
        if (ev.key === '-' && ev.target === svgEl) { g.view.k = Math.max(0.1, g.view.k / 1.2); applyView(); }
    });
    svgEl.setAttribute('tabindex', '0');
    filter.addEventListener('input', applyFilter);
    showCode.addEventListener('change', () => applyCode(showCode.checked));
    position();
    requestAnimationFrame(() => { fit(); applyView(); });
    void reducedMotion;
}
