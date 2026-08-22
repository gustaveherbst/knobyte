// Knobyte Project Hub client.
// All data comes from the Hub JSON API. Untrusted strings are only ever
// inserted with textContent / DOM APIs; innerHTML is never used.
'use strict';

(function () {
    const CSRF = (document.querySelector('meta[name="knobyte-csrf"]') || {}).content || '';
    const SVG_NS = 'http://www.w3.org/2000/svg';
    const FEED_INTERVAL_MS = 4000;
    const TABS = ['fleet', 'understand', 'inbox', 'specs', 'relays', 'contributors', 'activity', 'groundings', 'mcp'];

    const state = {
        tab: 'fleet',
        overview: null,
        loaded: {},
        feed: new Map(),
        feedFilter: 'all',
        feedUnseen: 0,
        feedReady: false,
        inboxFilter: 'pending',
        inbox: null,
        selectedProposal: null,
        relays: null,
        selectedRelay: null,
        specs: null,
        selectedSpec: null,
        contributors: [],
        graph: null,
        lastPreview: null,
    };

    // ------------------------------------------------------------------
    // DOM helpers
    // ------------------------------------------------------------------

    /** Create an element. attrs: class, text, title, dataset, on<Event>, other attributes. */
    function h(tag, attrs, ...children) {
        const el = document.createElement(tag);
        applyAttrs(el, attrs);
        appendChildren(el, children);
        return el;
    }

    function s(tag, attrs, ...children) {
        const el = document.createElementNS(SVG_NS, tag);
        applyAttrs(el, attrs);
        appendChildren(el, children);
        return el;
    }

    function applyAttrs(el, attrs) {
        if (!attrs) return;
        for (const [k, v] of Object.entries(attrs)) {
            if (v === undefined || v === null || v === false) continue;
            if (k === 'class') el.setAttribute('class', v);
            else if (k === 'text') el.textContent = String(v);
            else if (k === 'dataset') Object.assign(el.dataset, v);
            else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
            else if (k === 'style') throw new Error('inline style attributes are not allowed (CSP)');
            else el.setAttribute(k, v === true ? '' : String(v));
        }
    }

    function appendChildren(el, children) {
        for (const c of children.flat(Infinity)) {
            if (c === undefined || c === null || c === false) continue;
            el.appendChild(c instanceof Node ? c : document.createTextNode(String(c)));
        }
    }

    function $(id) { return document.getElementById(id); }
    function clear(el) { el.replaceChildren(); return el; }
    function fill(el, ...children) { el.replaceChildren(); appendChildren(el, children); return el; }

    function empty(text, ...extra) { return h('div', { class: 'empty-state' }, text, ...extra); }

    function fmtTime(ts) {
        if (!ts) return '—';
        return String(ts).substring(0, 19).replace('T', ' ');
    }

    function fmtNum(n) { return typeof n === 'number' ? n.toLocaleString() : '—'; }

    function badge(text, kind) { return h('span', { class: 'badge badge-' + (kind || 'neutral'), text: text }); }

    function statusBadge(status) {
        const map = {
            pending: 'warning', approved: 'success', rejected: 'danger',
            published: 'accent', acknowledged: 'warning', closed: 'neutral',
            healthy: 'success', warning: 'warning', drifted: 'danger', unavailable: 'neutral',
            active: 'success', draft: 'neutral',
        };
        return badge(status || 'unknown', map[status] || 'neutral');
    }

    function kindBadge(kind) {
        const k = String(kind || '').toLowerCase();
        const cls = k === 'decision' ? 'decision' : k === 'discovery' ? 'discovery' : k === 'risk' ? 'risk' : k.startsWith('team:') ? 'accent' : 'neutral';
        return badge(kind || 'event', cls);
    }

    function initials(name) {
        const parts = String(name || '').split(/\s+/).filter(Boolean);
        const i = parts.map(p => p[0]).join('').substring(0, 2).toUpperCase();
        return i || '?';
    }

    let toastTimer = null;
    function toast(msg, kind) {
        const t = $('toast');
        t.textContent = msg;
        t.className = 'show' + (kind ? ' ' + kind : '');
        clearTimeout(toastTimer);
        toastTimer = setTimeout(() => { t.className = ''; }, 4200);
    }

    // ------------------------------------------------------------------
    // API
    // ------------------------------------------------------------------

    async function api(path, opts) {
        const res = await fetch(path, Object.assign({ credentials: 'same-origin', headers: { Accept: 'application/json' } }, opts || {}));
        let data = null;
        try { data = await res.json(); } catch (_) { data = null; }
        if (!res.ok) {
            const msg = (data && data.error) || ('Request failed (' + res.status + ')');
            const err = new Error(msg);
            err.status = res.status;
            throw err;
        }
        return data;
    }

    function post(path, body) {
        return api(path, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', Accept: 'application/json', 'X-Knobyte-CSRF': CSRF },
            body: JSON.stringify(body || {}),
        });
    }

    function enc(v) { return encodeURIComponent(v); }

    // ------------------------------------------------------------------
    // Tabs & drawer
    // ------------------------------------------------------------------

    const loaders = {
        fleet: loadFleet,
        understand: loadUnderstand,
        inbox: loadInbox,
        specs: loadSpecs,
        relays: loadRelays,
        contributors: loadContributors,
        activity: () => { state.feedUnseen = 0; setCount('count-activity', 0); renderFeed(); },
        groundings: loadDrift,
        mcp: renderMcp,
    };

    function switchTab(tab, opts) {
        if (!TABS.includes(tab)) tab = 'fleet';
        state.tab = tab;
        document.querySelectorAll('.nav-item').forEach(el => el.classList.toggle('active', el.dataset.tab === tab));
        document.querySelectorAll('.view-panel').forEach(el => el.classList.toggle('active-view', el.id === 'view-' + tab));
        if (window.history && window.history.replaceState) window.history.replaceState(null, '', '#' + tab);
        const force = opts && opts.reload;
        if (!state.loaded[tab] || force || tab === 'activity') {
            state.loaded[tab] = true;
            Promise.resolve(loaders[tab]()).catch(err => toast(err.message, 'error'));
        }
    }

    function setCount(id, n) {
        const el = $(id);
        if (!el) return;
        el.textContent = n > 0 ? String(n > 99 ? '99+' : n) : '';
        el.classList.toggle('has', n > 0);
    }

    function openDrawer(title, sub, ...body) {
        $('drawer-title').textContent = title;
        $('drawer-sub').textContent = sub || '';
        fill($('drawer-body'), ...body);
        $('drawer-overlay').classList.add('active');
        const d = $('drawer');
        d.classList.add('active');
        d.setAttribute('aria-hidden', 'false');
    }

    function closeDrawer() {
        $('drawer-overlay').classList.remove('active');
        const d = $('drawer');
        d.classList.remove('active');
        d.setAttribute('aria-hidden', 'true');
    }

    // ------------------------------------------------------------------
    // Overview (topbar, sidebar, counts)
    // ------------------------------------------------------------------

    async function loadOverview() {
        const o = await api('/api/overview');
        state.overview = o;
        const sb = $('storage-badge');
        if (o.graph) {
            sb.textContent = 'graph.db · ' + (o.graphJournalMode || 'sqlite').toUpperCase();
            sb.className = 'badge badge-success';
        } else {
            sb.textContent = 'graph not built';
            sb.className = 'badge badge-warning';
        }
        $('bind-pill').textContent = o.bind ? 'Hub · ' + o.bind : 'Hub';
        const mp = $('member-pill');
        if (o.currentMember) {
            mp.textContent = 'Member: ' + o.currentMember.displayName;
            mp.className = 'status-pill badge-success';
        } else {
            mp.textContent = 'No member selected';
            mp.className = 'status-pill badge-warning';
            mp.title = 'Run `knobyte member select <id>` to review and claim as yourself';
        }
        setCount('count-inbox', o.inbox ? o.inbox.pending : 0);
        setCount('count-relays', o.relays ? o.relays.open : 0);
        return o;
    }

    function memberWarning(id, member) {
        const el = $(id);
        if (member) { el.hidden = true; return; }
        fill(el, 'No current member is selected for this checkout, so actions cannot be attributed. Run ',
            h('code', { text: 'knobyte member select <id>' }), ' (or ', h('code', { text: 'knobyte member add <id> --select' }), ') and reload.');
        el.hidden = false;
    }

    // ------------------------------------------------------------------
    // Fleet
    // ------------------------------------------------------------------

    function kpi(value, label, cls) {
        return h('div', { class: 'kpi-card' }, h('div', { class: 'kpi-val' + (cls ? ' ' + cls : ''), text: value }), h('div', { class: 'kpi-lbl', text: label }));
    }

    function donut(svgEl, segments, total) {
        clear(svgEl);
        svgEl.appendChild(s('circle', { cx: 21, cy: 21, r: 15.915, fill: 'transparent', stroke: '#232d3d', 'stroke-width': 5 }));
        if (!total) return;
        let offset = 25;
        for (const seg of segments) {
            if (!seg.value) continue;
            const pct = (seg.value / total) * 100;
            svgEl.appendChild(s('circle', {
                cx: 21, cy: 21, r: 15.915, fill: 'transparent', stroke: seg.color, 'stroke-width': 5,
                'stroke-dasharray': pct.toFixed(2) + ' ' + (100 - pct).toFixed(2), 'stroke-dashoffset': offset.toFixed(2),
            }));
            offset -= pct;
        }
        svgEl.appendChild(s('text', { x: 21, y: 22.5, 'text-anchor': 'middle', fill: '#f8fafc', 'font-size': 6, 'font-weight': 700, text: String(total) }));
    }

    async function loadFleet() {
        const data = await api('/api/fleet');
        const st = data.stats;
        donut($('fleet-donut'), [
            { value: st.healthyProjects, color: '#34d399' },
            { value: st.warningProjects, color: '#fbbf24' },
            { value: st.driftedProjects, color: '#f87171' },
            { value: st.unavailableProjects, color: '#64748b' },
        ], st.totalProjects);
        fill($('fleet-legend'),
            h('div', { class: 'legend-item' }, h('span', { class: 'legend-dot dot-success' }), 'Intact (' + st.healthyProjects + ')'),
            h('div', { class: 'legend-item' }, h('span', { class: 'legend-dot dot-warning' }), 'Warning (' + st.warningProjects + ')'),
            h('div', { class: 'legend-item' }, h('span', { class: 'legend-dot dot-danger' }), 'Drift (' + st.driftedProjects + ')'),
            h('div', { class: 'legend-item' }, h('span', { class: 'legend-dot dot-muted' }), 'Unavailable (' + st.unavailableProjects + ')'));
        const agg = st.aggregateDrift;
        fill($('fleet-kpis'),
            kpi(fmtNum(st.totalProjects), 'Total Projects'),
            kpi(fmtNum(st.healthyProjects), 'Healthy Grounded', 'val-success'),
            kpi(agg === null || agg === undefined ? '—' : agg.toFixed(1) + '%', 'Aggregate Drift Health', agg === null || agg === undefined ? 'val-muted' : agg >= 95 ? 'val-success' : agg >= 80 ? 'val-warning' : 'val-danger'),
            kpi(fmtNum(st.totalContributors), 'Contributors (this project)'),
            kpi(fmtNum(st.totalNodes), 'Indexed Symbols'));

        const grid = $('projects-grid');
        if (!data.projects.length) { fill(grid, empty('No projects registered yet.')); return; }
        fill(grid, data.projects.map(projectCard));
    }

    function projectCard(p) {
        const cls = 'project-card' + (p.is_current ? ' card-active' : '') + (p.available ? '' : ' card-unavailable');
        const healthCls = p.status === 'healthy' ? 'val-success' : p.status === 'warning' ? 'val-warning' : p.status === 'drifted' ? 'val-danger' : 'val-muted';
        const stat = (label, val, extra) => h('div', null, h('div', { class: 'stat-mini-label', text: label }), h('div', { class: 'stat-mini-val' + (extra ? ' ' + extra : ''), text: val }));
        return h('div', { class: cls },
            h('div', { class: 'project-card-header' },
                h('div', { class: 'contributor-meta' }, h('div', { class: 'project-title', text: p.name }), h('div', { class: 'project-path', text: p.path })),
                p.is_current ? badge('Active', 'accent') : p.available ? badge('Standby') : badge('Unavailable', 'danger')),
            h('div', { class: 'project-stats-grid' },
                stat('Drift Health', p.available ? p.drift_score.toFixed(1) + '%' : '—', healthCls),
                stat('AST Symbols', p.available ? fmtNum(p.node_count) : '—'),
                stat('Call Edges', p.available ? fmtNum(p.edge_count) : '—'),
                stat('Topics', p.available ? fmtNum(p.wiki_count) : '—')),
            h('div', { class: 'project-card-footer' },
                h('span', { class: 'mode-badge', text: p.mode }),
                p.available ? null : h('span', { text: 'Path or scaffold missing on disk' }),
                h('span', { text: 'Last touch: ' + fmtTime(p.last_active) })));
    }

    // ------------------------------------------------------------------
    // Contributors
    // ------------------------------------------------------------------

    async function loadContributors() {
        const data = await api('/api/contributors');
        state.contributors = data.contributors;
        const grid = $('contributors-grid');
        if (!data.contributors.length) {
            fill(grid, empty('No team members are registered for this project yet. Add yourself with ',
                h('code', { text: 'knobyte member add <id> --name "Your Name" --select' }), '.'));
            return;
        }
        fill(grid, data.contributors.map(c => {
            const total = c.decisions_count + c.discoveries_count + c.notes_count + c.relays_authored;
            const metric = (v, l) => h('div', null, h('span', { class: 'c-val', text: v }), h('span', { class: 'c-lbl', text: l }));
            return h('button', { type: 'button', class: 'contributor-card', onclick: () => openContributor(c) },
                h('div', { class: 'contributor-header' },
                    h('div', { class: 'avatar', text: initials(c.display_name) }),
                    h('div', { class: 'contributor-meta' }, h('div', { class: 'contributor-name', text: c.display_name }), h('div', { class: 'contributor-handle', text: '@' + c.git_alias })),
                    statusBadge(c.status)),
                h('div', { class: 'contributor-metrics' },
                    metric(c.decisions_count, 'Decisions'), metric(c.discoveries_count, 'Discoveries'),
                    metric(c.files_touched_count, 'Files'), metric(total, 'Total')),
                c.in_flight_relay
                    ? h('div', { class: 'contributor-task' }, h('span', { class: 'pulse-dot' }), 'In flight: ', h('strong', { text: c.in_flight_relay }))
                    : h('div', { class: 'contributor-task idle', text: 'No active relay claimed' }),
                h('div', { class: 'click-hint', text: 'Inspect breakdown & history →' }));
        }));
    }

    function openContributor(c) {
        const total = (c.decisions_count + c.discoveries_count + c.notes_count + c.relays_authored) || 1;
        const segs = [
            { v: c.decisions_count, color: '#38bdf8', label: 'Decisions' },
            { v: c.discoveries_count, color: '#fbbf24', label: 'Discoveries' },
            { v: c.relays_authored, color: '#34d399', label: 'Relays' },
            { v: c.notes_count, color: '#64748b', label: 'Notes' },
        ];
        const chart = s('svg', { width: 120, height: 120, viewBox: '0 0 42 42' },
            s('circle', { cx: 21, cy: 21, r: 15.915, fill: 'transparent', stroke: '#232d3d', 'stroke-width': 6 }));
        let off = 25;
        for (const sg of segs) {
            if (!sg.v) continue;
            const pct = (sg.v / total) * 100;
            chart.appendChild(s('circle', { cx: 21, cy: 21, r: 15.915, fill: 'transparent', stroke: sg.color, 'stroke-width': 6, 'stroke-dasharray': pct + ' ' + (100 - pct), 'stroke-dashoffset': off }));
            off -= pct;
        }
        const legend = h('div', null, segs.map(sg => h('div', null, h('span', { class: 'legend-dot', 'data-color': sg.color }), ' ', sg.label + ': ', h('strong', { text: sg.v }))));
        legend.querySelectorAll('[data-color]').forEach(d => { d.style.background = d.dataset.color; });

        const events = (c.recent_events || []).map(ev => h('div', { class: 'box' },
            h('div', { class: 'grounding-head' }, kindBadge(ev.kind), h('span', { class: 'muted mono', text: fmtTime(ev.timestamp) })),
            h('div', { class: 'detail-text', text: ev.summary }),
            ev.files && ev.files.length ? h('div', { class: 'file-tags' }, ev.files.map(f => h('span', { class: 'file-pill', text: f }))) : null));

        openDrawer(c.display_name, '@' + c.git_alias,
            h('div', { class: 'box' }, h('div', { class: 'label', text: 'Contribution breakdown' }), h('div', { class: 'breakdown' }, chart, legend)),
            c.in_flight_relay
                ? h('div', { class: 'box box-accent' }, h('div', { class: 'label', text: 'Active in-flight handoff' }), h('div', { class: 'detail-text', text: c.in_flight_relay }))
                : h('div', { class: 'box muted', text: 'No active relay claimed. Available for handoffs.' }),
            h('div', null, h('div', { class: 'label', text: 'Projects touched' }), h('div', { class: 'chips' }, c.projects.map(p => badge(p, 'accent')))),
            h('div', null, h('div', { class: 'label', text: 'Change history & audit trail' }),
                events.length ? h('div', { class: 'drawer-body' }, events) : h('div', { class: 'muted', text: 'No decisions or file touches recorded yet.' })));
    }

    // ------------------------------------------------------------------
    // Live audit feed
    // ------------------------------------------------------------------

    function feedRow(item) {
        const files = (item.files || []).slice(0, 6);
        return h('div', { class: 'activity-row', dataset: { id: item.id, kind: String(item.kind || '').toLowerCase(), ts: item.timestamp || '' } },
            h('div', { class: 'act-col-actor' }, h('div', { class: 'actor-title', text: '@' + (item.actor || 'unattributed') }), kindBadge(item.kind)),
            h('div', { class: 'act-col-body' },
                h('div', { class: 'act-summary', text: item.summary }),
                item.details ? h('div', { class: 'act-details', text: item.details }) : null,
                files.length ? h('div', { class: 'file-tags' }, files.map(f => h('span', { class: 'file-pill', text: f }))) : null),
            h('div', { class: 'act-col-time', text: fmtTime(item.timestamp) }));
    }

    function feedVisible(row) {
        const f = state.feedFilter;
        if (f === 'all') return true;
        const k = row.dataset.kind;
        return f === 'team' ? k.startsWith('team:') : k === f;
    }

    function renderFeed() {
        const container = $('activity-feed');
        const items = Array.from(state.feed.values()).sort((a, b) => (b.item.timestamp || '').localeCompare(a.item.timestamp || ''));
        if (!items.length) {
            fill(container, empty('No recorded activity yet. Run ', h('code', { text: 'knobyte log <message>' }), ' to record a decision.'));
            return;
        }
        if (container.querySelector('.empty-state')) clear(container);
        // Re-order existing nodes only when needed: appendChild moves nodes without re-creating them.
        let prev = null;
        for (const entry of items) {
            const row = entry.row;
            row.hidden = !feedVisible(row);
            const expectedNext = prev ? prev.nextSibling : container.firstChild;
            if (expectedNext !== row) container.insertBefore(row, expectedNext);
            prev = row;
        }
    }

    async function pollFeed() {
        try {
            const data = await api('/api/feed?limit=150');
            const seen = new Set();
            let fresh = 0;
            for (const item of data.items) {
                seen.add(item.id);
                if (!state.feed.has(item.id)) {
                    const row = feedRow(item);
                    if (state.feedReady) {
                        row.classList.add('fresh');
                        setTimeout(() => row.classList.remove('fresh'), 2500);
                        fresh++;
                    }
                    state.feed.set(item.id, { item, row });
                }
            }
            for (const [id, entry] of state.feed) {
                if (!seen.has(id)) { entry.row.remove(); state.feed.delete(id); }
            }
            state.feedReady = true;
            if (fresh && state.tab !== 'activity') {
                state.feedUnseen += fresh;
                setCount('count-activity', state.feedUnseen);
            }
            if (fresh) {
                // Keep counters elsewhere in sync with new team activity.
                loadOverview().catch(() => {});
            }
            renderFeed();
            $('live-text').textContent = 'Live · ' + new Date().toLocaleTimeString([], { hour12: false });
            document.querySelector('#live-pill .pulse-dot').classList.remove('off');
        } catch (err) {
            $('live-text').textContent = 'Feed offline';
            document.querySelector('#live-pill .pulse-dot').classList.add('off');
        }
    }

    function startFeed() {
        pollFeed();
        setInterval(() => { if (!document.hidden) pollFeed(); }, FEED_INTERVAL_MS);
        document.addEventListener('visibilitychange', () => { if (!document.hidden) pollFeed(); });
    }

    // ------------------------------------------------------------------
    // Understand the project: context graph
    // ------------------------------------------------------------------

    async function loadUnderstand() {
        const o = state.overview || await loadOverview();
        const g = o.graph;
        fill($('ws-kpis'),
            kpi(o.drift.score.toFixed(1) + '%', 'Grounding Score', o.drift.score >= 95 ? 'val-success' : o.drift.score >= 80 ? 'val-warning' : 'val-danger'),
            kpi(fmtNum(o.drift.fileCount), 'Scaffold Context Files'),
            kpi(g ? fmtNum(g.node_count) : '—', 'AST Code Nodes'),
            kpi(g ? fmtNum(g.edge_count) : '—', 'Call / Dependency Edges'),
            kpi(fmtNum(o.wikiCount), 'Wiki Entities'),
            kpi(fmtNum(o.relays.total), 'Relays'));
        const data = await api('/api/graph/context');
        state.graph = buildGraph(data);
        renderGraph();
    }

    function buildGraph(data) {
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
        return { nodes, edges, byId, hint: data.hint, truncated: data.truncated, view: { x: 0, y: 0, k: 1 }, selected: null };
    }

    // Deterministic force-directed layout (Fruchterman-Reingold style).
    function layout(nodes, edges) {
        const n = nodes.length;
        if (!n) return;
        const golden = Math.PI * (3 - Math.sqrt(5));
        nodes.forEach((nd, i) => {
            const r = 40 * Math.sqrt(i + 1);
            nd.x = r * Math.cos(i * golden);
            nd.y = r * Math.sin(i * golden);
        });
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
                    a.vx += dx * f; a.vy += dy * f;
                    b.vx -= dx * f; b.vy -= dy * f;
                }
            }
            for (const e of edges) {
                const dx = e.a.x - e.b.x, dy = e.a.y - e.b.y;
                const d = Math.sqrt(dx * dx + dy * dy) || 0.01;
                const ideal = e.kind === 'grounds_to' ? kOpt * 0.6 : kOpt;
                const f = (d - ideal) / d * 0.5;
                e.a.vx -= dx * f; e.a.vy -= dy * f;
                e.b.vx += dx * f; e.b.vy += dy * f;
            }
            for (const v of nodes) {
                // gentle gravity keeps disconnected components close
                v.vx -= v.x * 0.01; v.vy -= v.y * 0.01;
                const d = Math.sqrt(v.vx * v.vx + v.vy * v.vy) || 0.01;
                const step = Math.min(d, temp);
                v.x += (v.vx / d) * step;
                v.y += (v.vy / d) * step;
            }
            temp *= 0.97;
        }
    }

    function truncate(str, n) { str = String(str || ''); return str.length > n ? str.substring(0, n - 1) + '…' : str; }

    function renderGraph() {
        const g = state.graph;
        const svgEl = $('graph-svg');
        clear(svgEl);
        const emptyEl = $('graph-empty');
        if (!g || !g.nodes.length) {
            fill(emptyEl, (g && g.hint) || 'No wiki entities indexed yet. Add Markdown under .knobyte/ and run `knobyte wiki rebuild-index`.');
            emptyEl.hidden = false;
            return;
        }
        emptyEl.hidden = true;
        const showCode = $('graph-show-code').checked;
        const viewport = s('g', { class: 'viewport' });
        const edgeLayer = s('g'), nodeLayer = s('g');
        viewport.append(edgeLayer, nodeLayer);
        svgEl.appendChild(viewport);

        for (const e of g.edges) {
            e.el = s('line', { class: 'g-edge' + (e.kind === 'grounds_to' ? ' grounds' : '') }, s('title', { text: e.kind }));
            edgeLayer.appendChild(e.el);
        }
        for (const nd of g.nodes) {
            const r = nd.group === 'entity' ? 7 + Math.min(nd.deg, 10) * 0.6 : 5.5;
            const shape = nd.group === 'code'
                ? s('rect', { class: 'shape', x: -r, y: -r, width: r * 2, height: r * 2, rx: 2 })
                : s('circle', { class: 'shape', r: r });
            const title = nd.group === 'entity' ? nd.label + ' (' + nd.entityType + ')' : nd.group === 'code' ? nd.label + ' — ' + nd.file : 'Unresolved grounding: ' + nd.nodeId;
            nd.el = s('g', { class: 'g-node ' + nd.group, tabindex: 0, role: 'button', 'aria-label': title },
                s('title', { text: title }), shape, s('text', { x: r + 4, y: 3.5, text: truncate(nd.label, 26) }));
            nd.el.addEventListener('pointerdown', ev => startNodeDrag(ev, nd));
            nd.el.addEventListener('keydown', ev => { if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); selectGraphNode(nd); } });
            nodeLayer.appendChild(nd.el);
        }
        g.viewport = viewport;
        applyCodeVisibility(showCode);
        positionGraph();
        if (!g.fitted) fitGraph();
        applyView();
        applyFilter();
        if (g.selected) highlight(g.selected);
    }

    function applyCodeVisibility(show) {
        const g = state.graph;
        if (!g) return;
        for (const nd of g.nodes) if (nd.group !== 'entity' && nd.el) nd.el.style.display = show ? '' : 'none';
        for (const e of g.edges) if (e.el) e.el.style.display = (!show && (e.a.group !== 'entity' || e.b.group !== 'entity')) ? 'none' : '';
    }

    function positionGraph() {
        const g = state.graph;
        for (const nd of g.nodes) nd.el.setAttribute('transform', 'translate(' + nd.x.toFixed(1) + ',' + nd.y.toFixed(1) + ')');
        for (const e of g.edges) {
            e.el.setAttribute('x1', e.a.x.toFixed(1)); e.el.setAttribute('y1', e.a.y.toFixed(1));
            e.el.setAttribute('x2', e.b.x.toFixed(1)); e.el.setAttribute('y2', e.b.y.toFixed(1));
        }
    }

    function fitGraph() {
        const g = state.graph;
        const wrap = $('graph-wrap');
        const w = wrap.clientWidth || 800, hgt = wrap.clientHeight || 500;
        let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
        for (const nd of g.nodes) { minX = Math.min(minX, nd.x); minY = Math.min(minY, nd.y); maxX = Math.max(maxX, nd.x); maxY = Math.max(maxY, nd.y); }
        const gw = Math.max(maxX - minX, 1) + 160, gh = Math.max(maxY - minY, 1) + 80;
        const k = Math.max(0.15, Math.min(2, Math.min(w / gw, hgt / gh)));
        g.view = { k, x: w / 2 - ((minX + maxX) / 2) * k, y: hgt / 2 - ((minY + maxY) / 2) * k };
        g.fitted = true;
    }

    function applyView() {
        const g = state.graph;
        if (g && g.viewport) g.viewport.setAttribute('transform', 'translate(' + g.view.x + ',' + g.view.y + ') scale(' + g.view.k + ')');
    }

    function applyFilter() {
        const g = state.graph;
        if (!g) return;
        const q = $('graph-filter').value.trim().toLowerCase();
        if (!q) {
            if (!g.selected) { g.nodes.forEach(n => n.el.classList.remove('dim')); g.edges.forEach(e => e.el.classList.remove('dim')); }
            return;
        }
        const match = nd => String(nd.label).toLowerCase().includes(q) || String(nd.file || '').toLowerCase().includes(q) || String(nd.entityId || nd.nodeId || '').toLowerCase().includes(q);
        g.nodes.forEach(n => n.el.classList.toggle('dim', !match(n)));
        g.edges.forEach(e => e.el.classList.toggle('dim', !(match(e.a) && match(e.b))));
    }

    function highlight(nd) {
        const g = state.graph;
        const neigh = new Set([nd]);
        for (const e of g.edges) {
            const on = e.a === nd || e.b === nd;
            e.el.classList.toggle('hl', on);
            if (on) { neigh.add(e.a); neigh.add(e.b); }
        }
        g.nodes.forEach(n => {
            n.el.classList.toggle('selected', n === nd);
            n.el.classList.toggle('dim', !neigh.has(n));
        });
        g.edges.forEach(e => e.el.classList.toggle('dim', !(neigh.has(e.a) && neigh.has(e.b))));
    }

    function selectGraphNode(nd) {
        state.graph.selected = nd;
        highlight(nd);
        if (nd.group === 'entity') showEntity(nd.entityId);
        else if (nd.group === 'code') showCodeNode(nd.nodeId);
        else showMissingGrounding(nd.nodeId);
    }

    function focusInGraph(prefix, id) {
        const g = state.graph;
        if (!g || !g.byId) return;
        const nd = g.byId.get(prefix + id);
        if (!nd || !nd.el) return;
        g.selected = nd;
        highlight(nd);
        const wrap = $('graph-wrap');
        g.view.x = wrap.clientWidth / 2 - nd.x * g.view.k;
        g.view.y = wrap.clientHeight / 2 - nd.y * g.view.k;
        applyView();
    }

    // Pan / zoom / drag
    let drag = null;
    function svgPoint(ev) {
        const rect = $('graph-svg').getBoundingClientRect();
        return { x: ev.clientX - rect.left, y: ev.clientY - rect.top };
    }
    function startNodeDrag(ev, nd) {
        ev.stopPropagation();
        const p = svgPoint(ev);
        drag = { type: 'node', nd, start: p, moved: false };
        $('graph-svg').setPointerCapture(ev.pointerId);
    }
    function initGraphInteractions() {
        const svgEl = $('graph-svg');
        svgEl.addEventListener('pointerdown', ev => {
            if (drag) return;
            const g = state.graph;
            if (!g) return;
            drag = { type: 'pan', start: svgPoint(ev), view: Object.assign({}, g.view), moved: false };
            svgEl.classList.add('panning');
            svgEl.setPointerCapture(ev.pointerId);
        });
        svgEl.addEventListener('pointermove', ev => {
            if (!drag || !state.graph) return;
            const g = state.graph;
            const p = svgPoint(ev);
            const dx = p.x - drag.start.x, dy = p.y - drag.start.y;
            if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
            if (drag.type === 'pan') {
                g.view.x = drag.view.x + dx; g.view.y = drag.view.y + dy;
                applyView();
            } else if (drag.moved) {
                drag.nd.x = (p.x - g.view.x) / g.view.k;
                drag.nd.y = (p.y - g.view.y) / g.view.k;
                positionGraph();
            }
        });
        const end = () => {
            if (!drag) return;
            if (drag.type === 'node' && !drag.moved) selectGraphNode(drag.nd);
            if (drag.type === 'pan' && !drag.moved && state.graph && state.graph.selected) {
                state.graph.selected = null;
                state.graph.nodes.forEach(n => n.el.classList.remove('selected', 'dim'));
                state.graph.edges.forEach(e => e.el.classList.remove('hl', 'dim'));
                applyFilter();
            }
            drag = null;
            svgEl.classList.remove('panning');
        };
        svgEl.addEventListener('pointerup', end);
        svgEl.addEventListener('pointercancel', end);
        svgEl.addEventListener('wheel', ev => {
            const g = state.graph;
            if (!g) return;
            ev.preventDefault();
            const p = svgPoint(ev);
            const factor = Math.exp(-ev.deltaY * 0.0015);
            const k = Math.max(0.1, Math.min(4, g.view.k * factor));
            g.view.x = p.x - (p.x - g.view.x) * (k / g.view.k);
            g.view.y = p.y - (p.y - g.view.y) * (k / g.view.k);
            g.view.k = k;
            applyView();
        }, { passive: false });
        $('graph-reset').addEventListener('click', () => { if (state.graph) { fitGraph(); applyView(); } });
        $('graph-filter').addEventListener('input', applyFilter);
        $('graph-show-code').addEventListener('change', ev => applyCodeVisibility(ev.target.checked));
    }

    // ------------------------------------------------------------------
    // Entity / code detail
    // ------------------------------------------------------------------

    function detailLoading() { fill($('detail-panel'), empty('Loading…')); }

    function codeBlock(snippet) {
        if (!snippet) return h('div', { class: 'muted', text: 'Source not available in this checkout.' });
        const lines = String(snippet.code).split('\n');
        const pre = h('pre', { class: 'code' }, lines.map((l, i) => h('span', { class: 'diff-line' }, h('span', { class: 'ln', text: snippet.startLine + i }), l)));
        return h('div', null, pre, snippet.truncated ? h('div', { class: 'muted', text: 'Truncated at line ' + snippet.endLine + '.' }) : null);
    }

    function astDetails(n) {
        const flags = [];
        if (n.is_exported) flags.push('exported');
        if (n.is_async) flags.push('async');
        if (n.is_static) flags.push('static');
        if (n.is_abstract) flags.push('abstract');
        const rows = [
            ['Kind', n.kind], ['Qualified name', n.qualified_name], ['File', n.file_path],
            ['Lines', n.start_line + '–' + n.end_line + ' (cols ' + n.start_column + '–' + n.end_column + ')'],
            ['Language', n.language], ['Visibility', n.visibility || '—'], ['Returns', n.return_type || '—'],
            ['Flags', flags.join(', ') || '—'], ['Node id', n.id],
        ];
        return h('dl', { class: 'kv' }, rows.map(([k, v]) => [h('dt', { text: k }), h('dd', { text: v })]));
    }

    function codeNodeCard(n, snippet, extra) {
        return h('div', { class: 'grounding-card' },
            h('div', { class: 'grounding-head' },
                h('span', { class: 'grounding-name', text: n.name }),
                h('span', { class: 'chips' }, badge(n.kind, 'success'), badge(n.file_path + ':' + n.start_line + '-' + n.end_line))),
            n.signature ? h('div', { class: 'signature', text: n.signature }) : null,
            n.docstring ? h('div', { class: 'docstring', text: n.docstring }) : null,
            h('details', null, h('summary', { class: 'muted', text: 'AST details' }), astDetails(n)),
            codeBlock(snippet),
            extra || null);
    }

    function entityChip(e) {
        return h('button', { type: 'button', class: 'chip chip-link', title: e.summary || e.title, onclick: () => { showEntity(e.id); focusInGraph('e:', e.id); }, text: e.title + ' · ' + e.type });
    }

    async function showEntity(id) {
        detailLoading();
        let d;
        try { d = await api('/api/wiki/entity?id=' + enc(id)); } catch (err) { fill($('detail-panel'), h('div', { class: 'notice notice-error', text: err.message })); return; }
        const e = d.entity;
        const groundings = d.groundings.map(gr => gr.resolved
            ? codeNodeCard(gr.node, gr.snippet, h('button', { type: 'button', class: 'btn btn-sm', text: 'Open code node', onclick: () => { showCodeNode(gr.nodeId); focusInGraph('c:', gr.nodeId); } }))
            : h('div', { class: 'grounding-card missing' },
                h('div', { class: 'grounding-head' }, h('span', { class: 'grounding-name', text: gr.nodeId }), badge('unresolved', 'danger')),
                h('div', { class: 'muted' }, d.graphAvailable ? 'This anchor no longer resolves to a code node. ' : 'Code graph not built. Run `knobyte graph rebuild`. ',
                    h('button', { type: 'button', class: 'btn btn-sm', text: 'Preview relocations', onclick: () => switchTab('groundings') }))));
        fill($('detail-panel'),
            h('div', { class: 'detail-title', text: e.title }),
            h('div', { class: 'detail-meta' }, badge(e.entity_type, 'accent'), statusBadge(e.status), h('span', { class: 'mono', text: e.file }), h('span', { text: 'rev ' + e.revision })),
            e.summary ? h('div', { class: 'detail-text', text: e.summary }) : null,
            e.topics && e.topics.length ? h('div', { class: 'detail-section chips' }, e.topics.map(t => badge(t))) : null,
            h('div', { class: 'detail-section' }, h('div', { class: 'label', text: 'Groundings (' + d.groundings.length + ')' }),
                groundings.length ? h('div', { class: 'drawer-body' }, groundings) : h('div', { class: 'muted', text: 'No code groundings declared.' })),
            h('div', { class: 'detail-section' }, h('div', { class: 'label', text: 'Relations' }),
                e.relations.length ? h('ul', { class: 'plain' }, e.relations.map(r => h('li', null, h('span', { class: 'muted', text: r.type + ' → ' }),
                    h('button', { type: 'button', class: 'chip chip-link', text: r.target_id, onclick: () => { showEntity(r.target_id); focusInGraph('e:', r.target_id); } }),
                    r.note ? h('span', { class: 'muted', text: ' — ' + r.note }) : null))) : h('div', { class: 'muted', text: 'None.' })),
            d.backlinks.length ? h('div', { class: 'detail-section' }, h('div', { class: 'label', text: 'Referenced by' }), h('div', { class: 'chips' }, d.backlinks.map(entityChip))) : null,
            d.related.length ? h('div', { class: 'detail-section' }, h('div', { class: 'label', text: 'Related' }), h('div', { class: 'chips' }, d.related.map(entityChip))) : null,
            h('details', { class: 'detail-section' }, h('summary', { class: 'muted', text: 'Markdown body' }), h('pre', { class: 'body', text: e.body })));
    }

    async function showCodeNode(id) {
        detailLoading();
        let d;
        try { d = await api('/api/code/node?id=' + enc(id)); } catch (err) { fill($('detail-panel'), h('div', { class: 'notice notice-error', text: err.message })); return; }
        const n = d.node;
        fill($('detail-panel'),
            h('div', { class: 'detail-title', text: n.qualified_name || n.name }),
            h('div', { class: 'detail-meta' }, badge(n.kind, 'success'), h('span', { class: 'mono', text: n.file_path + ':' + n.start_line })),
            codeNodeCard(n, d.snippet),
            h('div', { class: 'detail-section' }, h('div', { class: 'label', text: 'Explained by wiki entities' }),
                d.entities.length ? h('div', { class: 'chips' }, d.entities.map(entityChip)) : h('div', { class: 'muted', text: 'No wiki entity grounds this node.' })),
            h('div', { class: 'detail-section' }, h('div', { class: 'label', text: 'Callers (' + d.callers.length + ')' }),
                d.callers.length ? h('div', { class: 'chips' }, d.callers.map(c => h('button', { type: 'button', class: 'chip chip-link', title: c.file + ':' + c.startLine, text: c.name + ' · ' + c.file, onclick: () => showCodeNode(c.id) })))
                    : h('div', { class: 'muted', text: 'No recorded callers.' })));
    }

    function showMissingGrounding(id) {
        fill($('detail-panel'),
            h('div', { class: 'detail-title', text: 'Unresolved grounding' }),
            h('div', { class: 'detail-meta' }, h('span', { class: 'mono', text: id })),
            h('div', { class: 'detail-text', text: 'This anchor is declared by a wiki entity but no longer matches a node in the code graph. The symbol may have been moved or renamed.' }),
            h('div', { class: 'action-row' }, h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview relocations', onclick: () => switchTab('groundings') })));
    }

    // ------------------------------------------------------------------
    // Global search
    // ------------------------------------------------------------------

    let searchTimer = null, searchSeq = 0;
    function initSearch() {
        const input = $('global-search'), box = $('search-results');
        input.addEventListener('input', () => {
            clearTimeout(searchTimer);
            const q = input.value.trim();
            if (q.length < 2) { box.hidden = true; return; }
            searchTimer = setTimeout(() => runSearch(q), 220);
        });
        input.addEventListener('keydown', ev => { if (ev.key === 'Escape') { box.hidden = true; input.blur(); } });
        input.addEventListener('focus', () => { if (box.childElementCount && input.value.trim().length >= 2) box.hidden = false; });
        document.addEventListener('click', ev => { if (!ev.target.closest('.search-box')) box.hidden = true; });
    }

    async function runSearch(q) {
        const seq = ++searchSeq;
        const box = $('search-results');
        let d;
        try { d = await api('/api/search?q=' + enc(q)); } catch (err) { fill(box, h('div', { class: 'notice notice-error', text: err.message })); box.hidden = false; return; }
        if (seq !== searchSeq) return;
        const go = fn => () => { box.hidden = true; switchTab('understand'); fn(); };
        fill(box,
            h('div', { class: 'search-group-title', text: 'Wiki (' + d.wiki.length + ')' }),
            d.wiki.length ? d.wiki.map(e => h('button', { type: 'button', class: 'search-hit', onclick: go(() => { showEntity(e.id); focusInGraph('e:', e.id); }) },
                h('div', { class: 'search-hit-title', text: e.title }), h('div', { class: 'search-hit-sub', text: e.type + ' · ' + e.file + (e.summary ? ' — ' + truncate(e.summary, 90) : '') })))
                : h('div', { class: 'muted search-hit', text: 'No wiki matches.' }),
            h('div', { class: 'search-group-title', text: 'Code symbols (' + d.code.length + ')' }),
            d.code.length ? d.code.map(c => h('button', { type: 'button', class: 'search-hit', onclick: go(() => { showCodeNode(c.id); focusInGraph('c:', c.id); }) },
                h('div', { class: 'search-hit-title', text: c.name + '  ·  ' + c.kind }), h('div', { class: 'search-hit-sub', text: c.file + ':' + c.startLine + (c.signature ? ' — ' + truncate(c.signature, 90) : '') })))
                : h('div', { class: 'muted search-hit', text: 'No code matches (is the graph built?).' }));
        box.hidden = false;
    }

    // ------------------------------------------------------------------
    // Inbox
    // ------------------------------------------------------------------

    async function loadInbox() {
        state.inbox = await api('/api/inbox');
        memberWarning('inbox-member-warning', state.inbox.currentMember);
        renderInboxList();
        renderInboxDrafts();
        if (state.selectedProposal) showProposal(state.selectedProposal);
    }

    function renderInboxList() {
        const list = $('inbox-list');
        const f = state.inboxFilter;
        const items = state.inbox.proposals.filter(p => f === 'all' || p.status === f);
        if (!items.length) { fill(list, empty(f === 'pending' ? 'No pending proposals. Agents and teammates submit them with `knobyte inbox`.' : 'Nothing here.')); return; }
        fill(list, items.map(p => h('button', { type: 'button', class: 'list-item' + (p.id === state.selectedProposal ? ' selected' : ''), onclick: () => showProposal(p.id) },
            h('div', { class: 'list-item-title', text: p.title }),
            h('div', { class: 'list-item-meta' }, statusBadge(p.status), h('span', { class: 'mono', text: p.target }), h('span', { text: '@' + p.author }), h('span', { text: fmtTime(p.updatedAt) })))));
    }

    function renderInboxDrafts() {
        const el = $('inbox-drafts');
        const drafts = state.inbox.drafts;
        if (!drafts.length) { fill(el, empty('No local inbox drafts.')); return; }
        fill(el, drafts.map(d => h('div', { class: 'row-item' },
            h('div', { class: 'contributor-meta' }, h('div', { class: 'list-item-title', text: d.title }),
                h('div', { class: 'list-item-meta' }, h('span', { class: 'mono', text: d.target }), h('span', { text: '@' + d.author }), h('span', { text: fmtTime(d.createdAt) })),
                d.reason ? h('div', { class: 'muted', text: d.reason }) : null),
            h('button', { type: 'button', class: 'btn btn-sm btn-primary', text: 'Publish to inbox', onclick: ev => act(ev.currentTarget, '/api/inbox/drafts/' + enc(d.id) + '/publish', null, 'Draft published', loadInbox) }))));
    }

    function diffView(diff) {
        const out = [];
        let run = [];
        const flush = () => {
            if (run.length > 8) {
                out.push(...run.slice(0, 3).map(l => h('span', { class: 'diff-line' }, '  ' + l.text)));
                out.push(h('span', { class: 'diff-line diff-skip', text: '  … ' + (run.length - 6) + ' unchanged lines …' }));
                out.push(...run.slice(-3).map(l => h('span', { class: 'diff-line' }, '  ' + l.text)));
            } else out.push(...run.map(l => h('span', { class: 'diff-line' }, '  ' + l.text)));
            run = [];
        };
        for (const l of diff) {
            if (l.op === '=') { run.push(l); continue; }
            flush();
            out.push(h('span', { class: 'diff-line ' + (l.op === '+' ? 'diff-add' : 'diff-del') }, l.op + ' ' + l.text));
        }
        flush();
        return h('pre', { class: 'diff' }, out.length ? out : h('span', { class: 'diff-line diff-skip', text: 'No changes.' }));
    }

    async function showProposal(id) {
        state.selectedProposal = id;
        renderInboxList();
        const panel = $('inbox-detail');
        fill(panel, empty('Loading…'));
        let d;
        try { d = await api('/api/inbox/' + enc(id)); } catch (err) { fill(panel, h('div', { class: 'notice notice-error', text: err.message })); return; }
        const p = d.proposal;
        const pending = p.status === 'pending';
        const note = h('textarea', { class: 'input', placeholder: 'Optional review note…', maxlength: 4000, 'aria-label': 'Review note' });
        const msg = h('div');
        const decide = (verb) => async (ev) => {
            const btns = panel.querySelectorAll('.decide');
            btns.forEach(b => { b.disabled = true; });
            fill(msg);
            try {
                await post('/api/inbox/' + enc(id) + '/' + verb, { note: note.value });
                toast(verb === 'approve' ? 'Proposal approved and written to .knobyte/' + (d.target || p.target) : 'Proposal rejected', 'success');
                await loadInbox();
                loadOverview().catch(() => {});
                pollFeed();
            } catch (err) {
                fill(msg, h('div', { class: 'notice notice-error', text: err.message }));
                btns.forEach(b => { b.disabled = false; });
            }
            void ev;
        };
        const noMember = !state.inbox || !state.inbox.currentMember;
        fill(panel,
            h('div', { class: 'detail-title', text: p.title }),
            h('div', { class: 'detail-meta' }, statusBadge(p.status), badge(p.mode, 'accent'), h('span', { class: 'mono', text: '.knobyte/' + (d.target || p.target) }), h('span', { text: 'by @' + p.author }), h('span', { text: fmtTime(p.createdAt) })),
            p.reason ? h('div', { class: 'detail-text', text: p.reason }) : null,
            p.decisionBy ? h('div', { class: 'detail-section notice ' + (p.status === 'approved' ? 'notice-success' : 'notice-error') },
                p.status + ' by @' + p.decisionBy + ' at ' + fmtTime(p.decidedAt) + (p.decisionReason ? ' — ' + p.decisionReason : '')) : null,
            h('div', { class: 'detail-section' },
                h('div', { class: 'label', text: d.targetError ? 'Target error' : (d.targetExists ? 'Diff against current ' + d.target + ' (' + p.mode + ')' : 'New file ' + d.target) }),
                d.targetError ? h('div', { class: 'notice notice-error', text: d.targetError }) : diffView(d.diff)),
            h('details', { class: 'detail-section' }, h('summary', { class: 'muted', text: 'Proposed content' }), h('pre', { class: 'body', text: p.proposedContent })),
            pending ? h('div', { class: 'detail-section' },
                noMember ? h('div', { class: 'notice notice-warning', text: 'Select a current member (`knobyte member select <id>`) to approve or reject.' }) : null,
                note,
                h('div', { class: 'action-row' },
                    h('button', { type: 'button', class: 'btn btn-success decide', text: 'Approve', disabled: noMember, onclick: decide('approve') }),
                    h('button', { type: 'button', class: 'btn btn-danger decide', text: 'Reject', disabled: noMember, onclick: decide('reject') }),
                    !noMember ? h('span', { class: 'muted', text: 'as @' + state.inbox.currentMember.id }) : null),
                msg) : null);
    }

    // generic action button helper
    async function act(btn, url, body, okMsg, after) {
        btn.disabled = true;
        try {
            await post(url, body);
            toast(okMsg, 'success');
            if (after) await after();
            loadOverview().catch(() => {});
            pollFeed();
        } catch (err) {
            toast(err.message, 'error');
            btn.disabled = false;
        }
    }

    // ------------------------------------------------------------------
    // Specs
    // ------------------------------------------------------------------

    async function loadSpecs() {
        const d = await api('/api/specs');
        state.specs = d.specs;
        renderSpecList();
    }

    function renderSpecList() {
        const list = $('specs-list');
        if (!state.specs.length) { fill(list, empty('No specs yet. Add Markdown files under .knobyte/specs/.')); return; }
        fill(list, state.specs.map(sp => h('button', { type: 'button', class: 'list-item' + (sp.id === state.selectedSpec ? ' selected' : ''), onclick: () => showSpec(sp.id) },
            h('div', { class: 'list-item-title', text: sp.title }),
            h('div', { class: 'list-item-meta' }, statusBadge(sp.status), h('span', { class: 'mono', text: sp.file })))));
    }

    async function showSpec(id) {
        state.selectedSpec = id;
        renderSpecList();
        const panel = $('spec-detail');
        fill(panel, empty('Loading…'));
        try {
            const sp = await api('/api/specs/' + enc(id));
            fill(panel,
                h('div', { class: 'detail-title', text: sp.title }),
                h('div', { class: 'detail-meta' }, statusBadge(sp.status), h('span', { class: 'mono', text: sp.file }), h('span', { class: 'mono', text: sp.id })),
                sp.summary ? h('div', { class: 'detail-text', text: sp.summary }) : null,
                h('div', { class: 'detail-section' }, h('pre', { class: 'body', text: sp.body })));
        } catch (err) { fill(panel, h('div', { class: 'notice notice-error', text: err.message })); }
    }

    // ------------------------------------------------------------------
    // Relays
    // ------------------------------------------------------------------

    async function loadRelays() {
        state.relays = await api('/api/relays');
        memberWarning('relay-member-warning', state.relays.currentMember);
        renderRelayList();
        renderRelayDrafts();
        if (state.selectedRelay) showRelay(state.selectedRelay);
    }

    function recipientsText(r) {
        return r.openToTeam ? 'open to team' : 'to ' + (r.namedRecipients || []).map(x => '@' + x).join(', ');
    }

    function renderRelayList() {
        const list = $('relays-list');
        const relays = state.relays.relays;
        if (!relays.length) { fill(list, empty('No published relays.')); return; }
        fill(list, relays.map(r => h('button', { type: 'button', class: 'list-item' + (r.id === state.selectedRelay ? ' selected' : ''), onclick: () => showRelay(r.id) },
            h('div', { class: 'list-item-title', text: r.title }),
            h('div', { class: 'list-item-meta' }, statusBadge(r.status), h('span', { text: 'from @' + r.sender }), h('span', { text: recipientsText(r) }),
                r.claimant ? h('span', { text: 'claimed by @' + r.claimant }) : null))));
    }

    function renderRelayDrafts() {
        const el = $('relay-drafts');
        const drafts = state.relays.drafts;
        if (!drafts.length) { fill(el, empty('No local relay drafts. Agents prepare them with the relay draft tool.')); return; }
        fill(el, drafts.map(d => h('div', { class: 'row-item' },
            h('div', { class: 'contributor-meta' },
                h('div', { class: 'list-item-title', text: d.title }),
                h('div', { class: 'list-item-meta' }, h('span', { text: 'from @' + d.sender }), h('span', { text: recipientsText(d) }), h('span', { text: fmtTime(d.createdAt) })),
                h('div', { class: 'muted', text: d.summary })),
            h('button', { type: 'button', class: 'btn btn-sm btn-primary', text: 'Publish', onclick: ev => act(ev.currentTarget, '/api/relays/drafts/' + enc(d.id) + '/publish', null, 'Relay published', loadRelays) }))));
    }

    function listSection(label, items) {
        return h('div', { class: 'detail-section' }, h('div', { class: 'label', text: label }),
            items && items.length ? h('ul', { class: 'plain' }, items.map(i => h('li', { text: i }))) : h('div', { class: 'muted', text: 'None recorded.' }));
    }

    async function showRelay(id) {
        state.selectedRelay = id;
        renderRelayList();
        const panel = $('relay-detail');
        fill(panel, empty('Loading…'));
        let r;
        try { r = await api('/api/relays/' + enc(id)); } catch (err) { fill(panel, h('div', { class: 'notice notice-error', text: err.message })); return; }
        const me = state.relays && state.relays.currentMember;
        const obs = r.observedState || {};
        fill(panel,
            h('div', { class: 'detail-title', text: r.title }),
            h('div', { class: 'detail-meta' }, statusBadge(r.status), h('span', { text: 'from @' + r.sender }), h('span', { text: recipientsText(r) }), r.claimant ? h('span', { text: 'claimed by @' + r.claimant }) : null),
            h('div', { class: 'detail-text', text: r.summary }),
            listSection('Progress', r.progress),
            listSection('Blockers', r.blockers),
            listSection('Next actions', r.nextActions),
            listSection('Evidence', r.evidence),
            h('div', { class: 'detail-section' }, h('div', { class: 'label', text: 'Observed repository state' }),
                h('dl', { class: 'kv' },
                    h('dt', { text: 'Branch' }), h('dd', { text: obs.branch || '—' }),
                    h('dt', { text: 'HEAD' }), h('dd', { text: obs.headCommit || '—' }),
                    h('dt', { text: 'Dirty tree' }), h('dd', { text: obs.dirtyTree ? 'yes' : 'no' }),
                    h('dt', { text: 'Observed at' }), h('dd', { text: fmtTime(obs.timestamp) }),
                    h('dt', { text: 'Created' }), h('dd', { text: fmtTime(r.createdAt) }),
                    h('dt', { text: 'Acknowledged' }), h('dd', { text: fmtTime(r.acknowledgedAt) }),
                    h('dt', { text: 'Closed' }), h('dd', { text: fmtTime(r.closedAt) }))),
            h('div', { class: 'action-row' },
                r.status === 'published' ? h('button', { type: 'button', class: 'btn btn-primary', text: 'Claim relay', disabled: !me, onclick: ev => act(ev.currentTarget, '/api/relays/' + enc(r.id) + '/claim', null, 'Relay claimed', loadRelays) }) : null,
                r.status !== 'closed' ? h('button', { type: 'button', class: 'btn btn-danger', text: 'Close relay', disabled: !me, onclick: ev => { if (window.confirm('Close relay "' + r.title + '"?')) act(ev.currentTarget, '/api/relays/' + enc(r.id) + '/close', null, 'Relay closed', loadRelays); } }) : null,
                me ? h('span', { class: 'muted', text: 'as @' + me.id }) : h('span', { class: 'muted', text: 'Select a current member to act on relays.' })));
    }

    // ------------------------------------------------------------------
    // Groundings / drift
    // ------------------------------------------------------------------

    async function loadDrift() {
        const d = await api('/api/drift');
        const g = d.grounding || {};
        fill($('drift-kpis'),
            kpi(d.score.toFixed(1) + '%', 'Grounding Score', d.score >= 95 ? 'val-success' : d.score >= 80 ? 'val-warning' : 'val-danger'),
            kpi(fmtNum(g.intact), 'Intact Anchors', 'val-success'),
            kpi(fmtNum(g.changed), 'Changed Anchors', g.changed ? 'val-warning' : ''),
            kpi(fmtNum(g.missing), 'Missing Anchors', g.missing ? 'val-danger' : ''),
            kpi(fmtNum(d.issue_count), 'Drift Issues'));
        const issues = $('drift-issues');
        if (!d.issues.length) { fill(issues, empty('No drift issues. Scaffold and code agree.')); return; }
        fill(issues, h('div', { class: 'table-wrap' }, h('table', { class: 'data' },
            h('thead', null, h('tr', null, h('th', { text: 'Severity' }), h('th', { text: 'File' }), h('th', { text: 'Symbol' }), h('th', { text: 'Message' }))),
            h('tbody', null, d.issues.map(i => h('tr', null,
                h('td', null, badge(i.severity, i.severity === 'error' ? 'danger' : 'warning')),
                h('td', { class: 'mono', text: i.file }), h('td', { class: 'mono', text: i.symbol || '—' }), h('td', { text: i.message })))))));
    }

    function renderSyncResult(res, after) {
        const r = res.result;
        const el = $('sync-results');
        const head = h('div', { class: 'notice ' + (r.success ? 'notice-success' : 'notice-error'), text: r.message });
        if (!r.proposals.length) { fill(el, head); return; }
        fill(el, head, h('div', { class: 'table-wrap' }, h('table', { class: 'data' },
            h('thead', null, h('tr', null, h('th', { text: 'Scaffold file' }), h('th', { text: 'Symbol' }), h('th', { text: 'From' }), h('th', { text: 'To' }), h('th', { text: 'Confidence' }), h('th', { text: 'Reason' }))),
            h('tbody', null, r.proposals.map(p => h('tr', null,
                h('td', { class: 'mono', text: p.scaffold_file }), h('td', { class: 'mono', text: p.symbol_name }),
                h('td', { class: 'mono', text: (p.old_file || '?') + '\n' + p.old_node_id }), h('td', { class: 'mono', text: p.new_file + '\n' + p.new_node_id }),
                h('td', { text: (p.confidence * 100).toFixed(0) + '%' }), h('td', { text: p.reason })))))),
            after ? h('div', { class: 'muted', text: 'Grounding score after apply: ' + after.score.toFixed(1) + '%' }) : null);
    }

    function initSync() {
        $('sync-preview').addEventListener('click', async ev => {
            const btn = ev.currentTarget;
            btn.disabled = true;
            $('sync-status').textContent = 'Scanning…';
            try {
                const res = await post('/api/drift/sync', { dryRun: true });
                state.lastPreview = res.result;
                renderSyncResult(res);
                $('sync-apply').disabled = !res.result.proposals.length;
                $('sync-status').textContent = 'Preview generated ' + new Date().toLocaleTimeString([], { hour12: false });
            } catch (err) {
                fill($('sync-results'), h('div', { class: 'notice notice-error', text: err.message }));
                $('sync-status').textContent = '';
            } finally { btn.disabled = false; }
        });
        $('sync-apply').addEventListener('click', async ev => {
            const btn = ev.currentTarget;
            const n = state.lastPreview ? state.lastPreview.proposals.length : 0;
            if (!window.confirm('Apply ' + n + ' grounding relocation(s)? This rewrites the affected scaffold Markdown files.')) return;
            btn.disabled = true;
            $('sync-status').textContent = 'Applying…';
            try {
                const res = await post('/api/drift/sync', { dryRun: false });
                renderSyncResult(res, res.driftAfter);
                toast(res.result.message, 'success');
                $('sync-status').textContent = 'Applied ' + new Date().toLocaleTimeString([], { hour12: false });
                state.lastPreview = null;
                await loadDrift();
                state.loaded.understand = false;
                loadOverview().catch(() => {});
            } catch (err) {
                fill($('sync-results'), h('div', { class: 'notice notice-error', text: err.message }));
                btn.disabled = false;
                $('sync-status').textContent = '';
            }
        });
    }

    // ------------------------------------------------------------------
    // MCP
    // ------------------------------------------------------------------

    async function renderMcp() {
        const o = state.overview || await loadOverview();
        const tools = o.mcpTools || [];
        $('mcp-title').textContent = tools.length + ' registered Knobyte MCP tools';
        fill($('mcp-tools'), tools.map(t => h('div', { class: 'tool-item' }, h('div', { class: 'tool-name', text: t.name }), h('div', { class: 'tool-desc', text: t.description }))));
    }

    // ------------------------------------------------------------------
    // Boot
    // ------------------------------------------------------------------

    function init() {
        document.querySelectorAll('.nav-item').forEach(el => el.addEventListener('click', () => switchTab(el.dataset.tab, { reload: el.dataset.tab !== 'understand' })));
        $('drawer-overlay').addEventListener('click', closeDrawer);
        $('drawer-close').addEventListener('click', closeDrawer);
        document.addEventListener('keydown', ev => { if (ev.key === 'Escape') closeDrawer(); });
        $('inbox-filters').addEventListener('click', ev => {
            const b = ev.target.closest('.chip');
            if (!b) return;
            state.inboxFilter = b.dataset.status;
            $('inbox-filters').querySelectorAll('.chip').forEach(c => c.classList.toggle('active', c === b));
            if (state.inbox) renderInboxList();
        });
        $('feed-filters').addEventListener('click', ev => {
            const b = ev.target.closest('.chip');
            if (!b) return;
            state.feedFilter = b.dataset.kind;
            $('feed-filters').querySelectorAll('.chip').forEach(c => c.classList.toggle('active', c === b));
            renderFeed();
        });
        initGraphInteractions();
        initSearch();
        initSync();

        loadOverview().catch(err => toast(err.message, 'error'));
        const hash = window.location.hash.replace('#', '');
        switchTab(TABS.includes(hash) ? hash : 'fleet');
        window.addEventListener('hashchange', () => {
            const h = window.location.hash.replace('#', '');
            if (TABS.includes(h)) switchTab(h);
        });
        startFeed();
    }

    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
    else init();
})();
