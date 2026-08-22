// Jobs: start index jobs (with rebuild confirmation), live phased progress
// over SSE, cancel, history.

import { h, fill, api, post, pageHeader, card, empty, statusBadge, fmtTime, link, stream, toast, confirmDialog, refreshShell, badge } from '../core.js';

const DESCRIPTIONS = {
    graph_refresh: 'Re-extract only changed files and publish atomically.',
    graph_rebuild: 'Parse every file from scratch and replace the code graph.',
    wiki_refresh: 'Index changed scaffold Markdown.',
    wiki_rebuild: 'Rebuild the wiki search index from scratch.',
    cozo_sync: 'Synchronize graph and wiki into CozoDB (vector search).',
    drift_check: 'Check scaffold claims against code, files and git.',
};

function progressBar(job) {
    const p = job.progress;
    const pct = p && p.total ? Math.min(100, Math.round((p.completed / p.total) * 100)) : null;
    const bar = h('div', { class: 'progress' + (pct === null && job.state === 'running' ? ' indeterminate' : ''), role: 'progressbar',
        'aria-valuemin': 0, 'aria-valuemax': 100, 'aria-valuenow': pct === null ? null : pct, 'aria-label': job.label + ' progress' },
        h('div', { class: 'progress-fill' }));
    if (pct !== null) bar.firstChild.dataset.pct = pct;
    if (pct !== null) bar.firstChild.style.width = pct + '%';
    else if (job.state !== 'running') bar.firstChild.style.width = job.state === 'succeeded' ? '100%' : '0%';
    return bar;
}

function phaseSteps(job) {
    const current = job.phases.indexOf(job.phase);
    const done = job.state === 'succeeded';
    return h('ol', { class: 'phases', 'aria-label': 'Phases' }, job.phases.map((p, i) => h('li', {
        class: 'phase' + (done || (current >= 0 && i < current) ? ' done' : '') + (i === current && !done ? ' current' : ''),
        'aria-current': i === current && !done ? 'step' : null,
        text: p,
    })));
}

/// A job card that updates itself live; `onTerminal(job)` fires once.
export function jobCard(job, opts) {
    opts = opts || {};
    const el = h('div', { class: 'job-card', 'aria-live': 'polite' });
    let closer = null;
    const draw = j => {
        fill(el,
            h('div', { class: 'job-head' },
                h('div', null, h('div', { class: 'job-title' }, link('/jobs/' + j.id, j.label)), h('div', { class: 'muted small', text: (DESCRIPTIONS[j.kind] || '') })),
                statusBadge(j.state)),
            h('div', { class: 'job-phase' }, h('strong', { text: j.phase }),
                j.progress && j.progress.message ? h('span', { class: 'muted', text: ' · ' + j.progress.message }) : null,
                j.progress && j.progress.total ? h('span', { class: 'muted mono', text: ' · ' + j.progress.completed + ' / ' + j.progress.total }) : null),
            phaseSteps(j),
            progressBar(j),
            j.summary ? h('div', { class: 'job-summary', text: j.summary }) : null,
            j.problem ? h('div', { class: 'notice notice-error', role: 'alert' }, h('strong', { text: j.problem.title }), ' ', j.problem.detail) : null,
            h('div', { class: 'job-foot' },
                h('span', { class: 'muted small', text: 'Started ' + fmtTime(j.startedAt || j.createdAt) + (j.finishedAt ? ' · finished ' + fmtTime(j.finishedAt) : '') }),
                !['succeeded', 'failed', 'interrupted'].includes(j.state)
                    ? h('button', { type: 'button', class: 'btn btn-sm', disabled: j.cancelRequested, text: j.cancelRequested ? 'Cancelling…' : 'Cancel', onclick: async ev => {
                        ev.currentTarget.disabled = true;
                        try { draw(await post('/api/jobs/' + j.id + '/cancel', {})); } catch (err) { toast(err.message, 'error'); }
                    } }) : null));
    };
    draw(job);
    if (!['succeeded', 'failed', 'interrupted'].includes(job.state)) {
        closer = stream('/api/jobs/' + job.id + '/events', {
            snapshot: draw,
            progress: draw,
            terminal: j => { draw(j); if (closer) closer(); refreshShell(); if (opts.onTerminal) opts.onTerminal(j); },
        });
    }
    el.stop = () => { if (closer) closer(); };
    return el;
}

/// Start a job; destructive kinds ask for confirmation first. Resolves with the job or null.
export async function startJob(kind, label) {
    const destructive = kind === 'graph_rebuild' || kind === 'wiki_rebuild';
    if (destructive) {
        const ok = await confirmDialog('Rebuild from scratch?',
            h('div', null,
                h('p', { class: 'dialog-text', text: (label || kind) + ' discards the current index and rebuilds it. A corrupt index is kept aside for recovery. Queries keep working on the old index until the new one is published.' }),
                h('p', { class: 'muted', text: 'Prefer a refresh unless the index is corrupt or the extractor changed.' })),
            { okText: 'Rebuild', danger: true });
        if (!ok) return null;
    }
    try {
        const job = await post('/api/jobs', { kind, confirm: destructive });
        toast((label || kind) + ' started', 'success');
        refreshShell();
        return job;
    } catch (err) {
        if (err.code === 'JOB_ALREADY_RUNNING' && err.problem && err.problem.activeJobId) {
            toast('Another job is running; wait for it or cancel it.', 'error');
        } else toast(err.message, 'error');
        return null;
    }
}

export async function page(ctx) {
    const { main, params } = ctx;
    const data = await api('/api/jobs?limit=50');
    const stops = [];
    ctx.cleanup(() => stops.forEach(f => f()));
    if (params.id) {
        const job = await api('/api/jobs/' + encodeURIComponent(params.id));
        const c = jobCard(job, { onTerminal: () => {} });
        stops.push(c.stop);
        fill(main, pageHeader(job.label, 'Job ' + job.id), link('/jobs', '← All jobs', 'back-link'), card(null, c),
            job.result ? card('Result', h('pre', { class: 'body', text: JSON.stringify(job.result, null, 2) })) : null);
        return;
    }
    const startRow = h('div', { class: 'job-kinds' }, data.kinds.map(k => h('button', {
        type: 'button', class: 'job-kind' + (k.requiresConfirmation ? ' destructive' : ''), disabled: !!data.active,
        onclick: async () => { const j = await startJob(k.kind, k.label); if (j) ctx.isCurrent() && page(ctx); },
    }, h('strong', { text: k.label }), h('span', { class: 'muted small', text: DESCRIPTIONS[k.kind] || '' }), k.requiresConfirmation ? badge('asks to confirm', 'warning') : null)));
    let active = null;
    if (data.active) {
        active = jobCard(data.active, { onTerminal: () => { if (ctx.isCurrent()) page(ctx); } });
        stops.push(active.stop);
    }
    const rows = data.items.filter(j => !data.active || j.id !== data.active.id);
    fill(main,
        pageHeader('Jobs', 'Index maintenance runs in the background, one job at a time. Cancelling stops at the next phase boundary, before anything is published.'),
        card('Active operation', active || empty('No job is running.')),
        card('Start a job', startRow, { aside: data.active ? h('span', { class: 'muted small', text: 'New jobs wait for the active one.' }) : null }),
        card('History', rows.length ? h('div', { class: 'table-wrap' }, h('table', { class: 'data' },
            h('thead', null, h('tr', null, ['Job', 'State', 'Phase', 'Started', 'Finished', 'Summary'].map(t => h('th', { scope: 'col', text: t })))),
            h('tbody', null, rows.map(j => h('tr', null,
                h('td', null, link('/jobs/' + j.id, j.label)), h('td', null, statusBadge(j.state), j.interruptedReason ? h('span', { class: 'muted small', text: ' ' + j.interruptedReason.replace(/_/g, ' ') }) : null),
                h('td', { text: j.phase }), h('td', { text: fmtTime(j.startedAt || j.createdAt) }), h('td', { text: fmtTime(j.finishedAt) }),
                h('td', { text: j.summary || (j.problem ? j.problem.detail : '') })))))) : empty('No jobs have run yet.')));
}
