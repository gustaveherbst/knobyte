// Overview (attention / next action, context readiness, latest team memory,
// active job) and the Fleet overview of every registered project.

import { h, s, fill, api, pageHeader, card, empty, badge, statusBadge, fmtTime, fmtNum, link, kv } from '../core.js';
import { jobCard } from './jobs.js';

function memoryItem(m) {
    return h('li', { class: 'memory-item' },
        h('div', { class: 'memory-head' }, badge(String(m.kind || 'event').replace(/^team:/, ''), m.source === 'activity' ? 'accent' : 'neutral'),
            h('span', { class: 'muted small', text: '@' + (m.actor || 'unattributed') + ' · ' + fmtTime(m.timestamp) })),
        h('div', { class: 'memory-summary', text: m.summary }),
        m.entity ? h('div', { class: 'muted small', text: m.entity }) : null,
        (m.files || []).length ? h('div', { class: 'file-tags' }, m.files.slice(0, 4).map(f => h('span', { class: 'file-pill', text: f }))) : null);
}

function readinessTile(label, status, detail, href) {
    return h('a', { class: 'tile', href, 'data-link': '' },
        h('div', { class: 'tile-label', text: label }),
        h('div', { class: 'tile-status' }, statusBadge(status)),
        h('div', { class: 'tile-detail muted small', text: detail }));
}

function coverageBar(ph) {
    const total = ph.total || 0;
    if (!total) return null;
    const seg = (n, cls) => { const d = h('span', { class: 'cov-seg ' + cls, title: n + ' files' }); d.style.width = (n / total * 100) + '%'; return d; };
    return h('div', { class: 'coverage' },
        h('div', { class: 'coverage-head' }, h('span', { class: 'muted small', text: 'Parse coverage' }), h('span', { class: 'mono small', text: ph.ok + '/' + total })),
        h('div', { class: 'cov-bar', role: 'img', 'aria-label': ph.ok + ' complete, ' + ph.partial + ' partial, ' + ph.failed + ' failed of ' + total },
            seg(ph.ok, 'ok'), seg(ph.partial, 'partial'), seg(ph.failed, 'failed')),
        h('div', { class: 'muted small', text: ph.ok + ' complete · ' + ph.partial + ' partial · ' + ph.failed + ' failed' }));
}

export async function home(ctx) {
    const { main } = ctx;
    const d = await api('/api/home');
    const r = d.readiness;
    const next = d.nextAction;
    const stops = [];
    ctx.cleanup(() => stops.forEach(f => f()));

    const attention = card('Attention', [
        next ? h('div', { class: 'next-action' },
            h('div', null, h('div', { class: 'eyebrow', text: 'Next action' }), h('div', { class: 'next-title', text: next.title }), h('div', { class: 'muted', text: next.detail })),
            link(next.href, next.action + ' →', 'btn btn-primary'))
            : h('div', { class: 'next-action calm' }, h('div', null, h('div', { class: 'eyebrow', text: 'All clear' }), h('div', { class: 'next-title', text: 'Nothing needs you right now' }),
                h('div', { class: 'muted', text: 'Context is fresh and no handoffs or reviews are waiting.' }))),
        d.attention.length > 1 ? h('ul', { class: 'attention-list' }, d.attention.slice(1).map(a => h('li', null,
            h('a', { href: a.href, 'data-link': '', class: 'attention-item' }, h('span', null, h('strong', { text: a.title }), h('span', { class: 'muted small', text: ' ' + a.detail })), h('span', { 'aria-hidden': 'true', text: '→' }))))) : null,
    ], { aside: link('/relays', 'View relays →', 'card-link') });

    const g = r.graph;
    const snap = r.indexedSnapshot;
    const readiness = card('Context readiness', [
        h('div', { class: 'tiles' },
            readinessTile('Repository', d.repo.available ? (d.repo.dirty ? 'degraded' : 'healthy') : 'unavailable',
                d.repo.available ? (d.repo.branch || 'detached') + ' · ' + (d.repo.headShort || '') + (d.repo.dirty ? ' · ' + d.repo.changedFiles + ' local changes' : '') : 'No git repository', '/health'),
            readinessTile('Knowledge', r.wiki.status, (r.wiki.entities || 0) + ' entities · ' + r.wiki.detail, '/health'),
            readinessTile('Code graph', g.status, fmtNum(g.counts.nodes) + ' symbols · ' + fmtNum(g.counts.files) + ' files · ' + g.changes + ' changed since index', '/health'),
            r.drift ? readinessTile('Drift', r.drift.issueCount ? 'warning' : 'healthy', 'Score ' + r.drift.score.toFixed(1) + ' · ' + r.drift.issueCount + ' issues', '/groundings') : null),
        kv([
            ['Repository now', (d.repo.branch || '—') + ' · ' + (d.repo.headShort || '—')],
            ['Indexed snapshot', snap ? (snap.branch || '—') + ' · ' + String(snap.head || '').substring(0, 10) + ' (' + fmtTime(snap.recordedAt) + ')' : 'not recorded by the Hub yet'],
            ['Graph last indexed', fmtTime(g.lastIndexedAt)],
            ['Changes since index', String(g.changes)],
        ]),
        coverageBar(g.parseHealth),
        h('div', { class: 'card-foot' }, link('/health', 'Open full health details →', 'card-link')),
    ], { aside: statusBadge(g.status === 'fresh' && r.wiki.status === 'fresh' ? 'healthy' : 'degraded') });

    const memory = card('Latest team memory', d.latestMemory.length ? h('ul', { class: 'memory-list' }, d.latestMemory.map(memoryItem)) : empty('No decisions or team activity recorded yet.'),
        { aside: link('/activity', 'View activity →', 'card-link') });

    let activeEl = empty('No index job is running.');
    if (d.activeJob) {
        activeEl = jobCard(d.activeJob, { onTerminal: () => { if (ctx.isCurrent()) home(ctx); } });
        stops.push(activeEl.stop);
    }
    fill(main,
        pageHeader('Overview', 'Last checked ' + fmtTime(new Date().toISOString()),
            link('/context', 'Explore context', 'btn btn-primary'),
            h('button', { type: 'button', class: 'btn', text: 'Refresh', onclick: () => home(ctx) })),
        h('div', { class: 'overview-grid' },
            h('div', { class: 'col-main' }, attention, readiness),
            h('div', { class: 'col-side' }, memory)),
        card('Active operation', activeEl, { aside: link('/jobs', 'All jobs →', 'card-link') }));
}

// ---------------------------------------------------------------------------
// Fleet
// ---------------------------------------------------------------------------

function kpi(value, label, cls) {
    return h('div', { class: 'kpi' }, h('div', { class: 'kpi-val' + (cls ? ' ' + cls : ''), text: value }), h('div', { class: 'kpi-lbl', text: label }));
}

function donut(segments, total) {
    const svg = s('svg', { width: 140, height: 140, viewBox: '0 0 42 42', role: 'img', 'aria-label': 'Fleet health' },
        s('circle', { cx: 21, cy: 21, r: 15.915, fill: 'transparent', class: 'donut-track', 'stroke-width': 5 }));
    let offset = 25;
    for (const seg of segments) {
        if (!seg.value || !total) continue;
        const pct = (seg.value / total) * 100;
        svg.appendChild(s('circle', { cx: 21, cy: 21, r: 15.915, fill: 'transparent', class: 'donut-seg ' + seg.cls, 'stroke-width': 5,
            'stroke-dasharray': pct.toFixed(2) + ' ' + (100 - pct).toFixed(2), 'stroke-dashoffset': offset.toFixed(2) }));
        offset -= pct;
    }
    svg.appendChild(s('text', { x: 21, y: 22.5, 'text-anchor': 'middle', class: 'donut-text', 'font-size': 6, 'font-weight': 700, text: String(total) }));
    return svg;
}

function projectCard(p) {
    const healthCls = p.status === 'healthy' ? 'val-success' : p.status === 'warning' ? 'val-warning' : p.status === 'drifted' ? 'val-danger' : 'val-muted';
    const stat = (label, val, extra) => h('div', null, h('div', { class: 'stat-label', text: label }), h('div', { class: 'stat-val' + (extra ? ' ' + extra : ''), text: val }));
    return h('article', { class: 'project-card' + (p.is_current ? ' current' : '') + (p.available ? '' : ' unavailable') },
        h('div', { class: 'project-head' },
            h('div', null, h('h3', { class: 'project-title', text: p.name }), h('div', { class: 'mono muted small', text: p.path })),
            p.is_current ? badge('This project', 'accent') : p.available ? badge('Standby') : badge('Unavailable', 'danger')),
        h('div', { class: 'project-stats' },
            stat('Drift health', p.available ? p.drift_score.toFixed(1) + '%' : '—', healthCls),
            stat('Symbols', p.available ? fmtNum(p.node_count) : '—'),
            stat('Edges', p.available ? fmtNum(p.edge_count) : '—'),
            stat('Topics', p.available ? fmtNum(p.wiki_count) : '—')),
        h('div', { class: 'project-foot muted small' }, h('span', { class: 'badge badge-neutral', text: p.mode }), h('span', { text: 'Last touch: ' + fmtTime(p.last_active) })));
}

export async function fleet(ctx) {
    const data = await api('/api/fleet');
    const st = data.stats;
    const agg = st.aggregateDrift;
    fill(ctx.main,
        pageHeader('Fleet', h('span', null, 'Knobyte repositories registered in ', h('code', { text: '~/.knobyte/projects.json' }), ' (plus sibling checkouts).')),
        h('div', { class: 'fleet-banner card' },
            h('div', { class: 'fleet-chart' }, donut([
                { value: st.healthyProjects, cls: 'ok' }, { value: st.warningProjects, cls: 'warn' },
                { value: st.driftedProjects, cls: 'bad' }, { value: st.unavailableProjects, cls: 'muted' }], st.totalProjects),
                h('ul', { class: 'legend' },
                    h('li', null, h('span', { class: 'legend-dot ok' }), 'Intact (' + st.healthyProjects + ')'),
                    h('li', null, h('span', { class: 'legend-dot warn' }), 'Warning (' + st.warningProjects + ')'),
                    h('li', null, h('span', { class: 'legend-dot bad' }), 'Drift (' + st.driftedProjects + ')'),
                    h('li', null, h('span', { class: 'legend-dot muted' }), 'Unavailable (' + st.unavailableProjects + ')'))),
            h('div', { class: 'kpis' },
                kpi(fmtNum(st.totalProjects), 'Projects'),
                kpi(fmtNum(st.healthyProjects), 'Healthy', 'val-success'),
                kpi(agg === null || agg === undefined ? '—' : agg.toFixed(1) + '%', 'Aggregate drift health', agg === null || agg === undefined ? 'val-muted' : agg >= 95 ? 'val-success' : agg >= 80 ? 'val-warning' : 'val-danger'),
                kpi(fmtNum(st.totalContributors), 'Contributors'),
                kpi(fmtNum(st.totalNodes), 'Indexed symbols'))),
        data.projects.length ? h('div', { class: 'project-grid' }, data.projects.map(projectCard)) : empty('No projects registered yet.'));
}

