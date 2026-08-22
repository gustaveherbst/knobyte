// Health: per-service cards (git, code graph, wiki, Cozo / embeddings, Hub),
// indexed-vs-HEAD delta, changed paths, parse coverage / failures and
// Refresh / Rebuild actions (as background jobs).

import { h, fill, api, pageHeader, card, empty, badge, statusBadge, fmtTime, fmtNum, link, kv } from '../core.js';
import { startJob, jobCard } from './jobs.js';

const JOB_LABEL = { graph_refresh: 'Refresh graph', graph_rebuild: 'Rebuild graph', wiki_refresh: 'Wiki refresh', wiki_rebuild: 'Wiki rebuild', cozo_sync: 'Sync Cozo' };

function jobButtons(kinds, recommended, active, after) {
    return h('div', { class: 'action-row' }, kinds.map(k => h('button', {
        type: 'button', class: 'btn btn-sm' + (k === recommended ? ' btn-primary' : ''), disabled: !!active,
        onclick: async () => { if (await startJob(k, JOB_LABEL[k])) after(); },
    }, JOB_LABEL[k], k === recommended ? h('span', { class: 'visually-hidden', text: ' (recommended)' }) : null)),
    active ? h('span', { class: 'muted small', text: 'New operations wait for the active job.' }) : null);
}

function pathList(title, paths, total, cls) {
    if (!paths || !paths.length) return null;
    return h('div', { class: 'path-box ' + (cls || '') },
        h('strong', { text: title }),
        h('ul', { class: 'plain mono small' }, paths.slice(0, 12).map(p => h('li', { text: p }))),
        total > Math.min(paths.length, 12) ? h('div', { class: 'muted small', text: (total - Math.min(paths.length, 12)) + ' more not shown.' }) : null);
}

function graphCard(svc, active, after) {
    const g = svc.graph;
    const ph = g.parse_health;
    const ch = g.changes;
    const snap = svc.indexedSnapshot;
    const total = ph.total || 0;
    const bar = h('div', { class: 'cov-bar', role: 'img', 'aria-label': ph.ok + ' of ' + total + ' parsed' });
    if (total) for (const [n, cls] of [[ph.ok, 'ok'], [ph.partial, 'partial'], [ph.failed, 'failed']]) { const s = h('span', { class: 'cov-seg ' + cls }); s.style.width = (n / total * 100) + '%'; bar.appendChild(s); }
    return card('Code graph', [
        h('p', { class: 'muted', text: svc.detail }),
        h('div', { class: 'stat-grid' },
            h('div', { class: 'stat' }, h('div', { class: 'stat-label', text: 'Index status' }), statusBadge(g.status)),
            h('div', { class: 'stat' }, h('div', { class: 'stat-label', text: 'Indexed snapshot' }),
                h('div', { class: 'mono small', text: snap ? (snap.branch || '—') + ' · ' + String(snap.head || '').substring(0, 10) : 'not recorded' })),
            h('div', { class: 'stat' }, h('div', { class: 'stat-label', text: 'Repository delta' }), h('div', { class: 'stat-val', text: String(ch.total) }),
                h('div', { class: 'muted small', text: ch.added.length + ' added · ' + ch.modified.length + ' modified · ' + ch.deleted.length + ' deleted' })),
            h('div', { class: 'stat' }, h('div', { class: 'stat-label', text: 'Last success' }), h('div', { class: 'small', text: fmtTime(g.last_successful_index_at) }))),
        h('div', { class: 'coverage' }, h('div', { class: 'coverage-head' }, h('span', { class: 'stat-label', text: 'Parse health' }), h('span', { class: 'mono small', text: ph.ok + '/' + total })),
            bar, h('div', { class: 'muted small', text: ph.ok + ' complete · ' + ph.partial + ' partial · ' + ph.failed + ' failed' })),
        kv([['Symbols', fmtNum(g.counts.nodes)], ['Relationships', fmtNum(g.counts.edges)], ['Files', fmtNum(g.counts.files)], ['Unresolved references', fmtNum(g.counts.unresolved)],
            ['Schema', g.schema_version], ['Extractor', g.extractor_version], ['Build mode', g.build_mode]]),
        svc.headMoved ? h('div', { class: 'notice notice-warning', text: 'HEAD moved since the Hub last indexed this graph.' }) : null,
        ch.policy_changed ? h('div', { class: 'notice notice-warning', text: 'The corpus policy changed since the last build.' }) : null,
        ch.extractor_changed ? h('div', { class: 'notice notice-warning', text: 'The extractor version changed; rebuild to re-extract everything.' }) : null,
        pathList('Added since index', ch.added, ch.added.length, 'warn'), pathList('Modified since index', ch.modified, ch.modified.length, 'warn'), pathList('Deleted since index', ch.deleted, ch.deleted.length, 'warn'),
        ch.truncated ? h('div', { class: 'muted small', text: 'Changed-path lists are truncated; totals are exact.' }) : null,
        pathList('Files with parse failures', ph.failed_paths, ph.failed, 'bad'),
        pathList('Partially parsed files', ph.partial_paths, ph.partial, ''),
        g.coverage && g.coverage.unindexed_total ? h('div', { class: 'muted small', text: 'Not indexed: ' + g.coverage.unindexed_total + ' file(s) with extensions no extractor handles (' + g.coverage.unindexed.slice(0, 6).map(u => u.extension + ' ' + u.files).join(', ') + ').' }) : null,
        (g.diagnostics || []).length ? h('ul', { class: 'plain' }, g.diagnostics.map(d => h('li', { class: 'notice notice-' + (d.severity === 'error' ? 'error' : 'warning') }, d.message, d.command ? h('code', { text: ' ' + d.command }) : null))) : null,
        jobButtons(['graph_refresh', 'graph_rebuild'], svc.recommendedJob, active, after),
    ], { aside: statusBadge(svc.status), class: 'service-main' });
}

function serviceCard(svc, active, after) {
    let body = [h('p', { class: 'muted', text: svc.detail })];
    if (svc.id === 'git') {
        const r = svc.repo;
        body.push(kv([['Branch', r.branch || (r.available ? 'detached' : '—')], ['HEAD', h('span', { class: 'mono small', text: r.head || '—' })], ['Working tree', r.available ? (r.dirty ? r.changedFiles + ' local change(s)' : 'clean') : '—']]));
    } else if (svc.id === 'wiki') {
        const w = svc.wiki;
        body.push(kv([['Index status', statusBadge(w.status)], ['Entities', fmtNum(w.entities)], ['Indexed', fmtTime(w.indexedAt)], ['Newest source', fmtTime(w.newestSourceAt)]]),
            jobButtons(['wiki_refresh', 'wiki_rebuild'], svc.recommendedJob, active, after));
    } else if (svc.id === 'cozo') {
        const e = svc.embedding;
        body.push(kv([['Embedding backend', badge(e.backend, e.backend === 'model2vec' ? 'accent' : 'neutral')], ['Model', e.model], ['Model present', e.model_present ? 'yes' : 'no'],
            ['Dimensions', e.dim], ['Embedder id', h('span', { class: 'mono small', text: e.embedder_id })], ['Cozo store', svc.dbExists ? 'synchronized at least once' : 'not created yet']]),
            jobButtons(['cozo_sync'], svc.recommendedJob, active, after));
    } else if (svc.id === 'hub') {
        body.push(kv([['Version', svc.version], ['Bound to', svc.bind], ['Active job', svc.activeJob ? link('/jobs/' + svc.activeJob.id, svc.activeJob.label) : 'none']]));
    }
    return h('section', { class: 'service', 'aria-label': svc.title },
        h('div', { class: 'service-head' }, h('h3', { class: 'service-title', text: svc.title }), statusBadge(svc.status)), body);
}

export async function page(ctx) {
    const { main } = ctx;
    const d = await api('/api/health');
    const stops = [];
    ctx.cleanup(() => stops.forEach(f => f()));
    const reload = () => { if (ctx.isCurrent()) page(ctx); };
    const graph = d.services.find(s => s.id === 'graph');
    const others = d.services.filter(s => s.id !== 'graph');
    let active = null;
    if (d.activeJob) { active = jobCard(d.activeJob, { onTerminal: reload }); stops.push(active.stop); }
    fill(main,
        pageHeader('Health', 'Checked ' + fmtTime(d.checkedAt), statusBadge(d.overall), h('button', { type: 'button', class: 'btn', text: 'Re-check', onclick: reload })),
        h('div', { class: 'health-summary card' }, statusBadge(d.overall), h('strong', { text: ' ' + d.services.length + ' local services' }),
            h('span', { class: 'muted small', text: ' · ' + d.services.filter(s => s.status === 'healthy').length + ' healthy' })),
        active ? card('Active operation', active, { aside: link('/jobs', 'All jobs →', 'card-link') }) : null,
        h('div', { class: 'health-grid' },
            graphCard(graph, d.activeJob, reload),
            card('Services', h('div', { class: 'services' }, others.map(s => serviceCard(s, d.activeJob, reload))), { aside: badge(String(others.length)) })),
        !graph ? empty('No graph service.') : null);
}
