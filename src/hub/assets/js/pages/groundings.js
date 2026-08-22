// Self-healing groundings: drift KPIs and issues, relocation preview (dry
// run) and a confirmed apply.

import { h, fill, api, post, pageHeader, card, empty, badge, fmtNum, toast, confirmDialog } from '../core.js';
import { startJob } from './jobs.js';

function kpi(value, label, cls) { return h('div', { class: 'kpi' }, h('div', { class: 'kpi-val' + (cls ? ' ' + cls : ''), text: value }), h('div', { class: 'kpi-lbl', text: label })); }

function results(res, after) {
    const r = res.result;
    const head = h('div', { class: 'notice ' + (r.success ? 'notice-success' : 'notice-error'), text: r.message });
    if (!r.proposals.length) return head;
    return [head, h('div', { class: 'table-wrap' }, h('table', { class: 'data' },
        h('thead', null, h('tr', null, ['Scaffold file', 'Symbol', 'From', 'To', 'Confidence', 'Reason'].map(t => h('th', { scope: 'col', text: t })))),
        h('tbody', null, r.proposals.map(p => h('tr', null,
            h('td', { class: 'mono', text: p.scaffold_file }), h('td', { class: 'mono', text: p.symbol_name }),
            h('td', { class: 'mono', text: (p.old_file || '?') + '\n' + p.old_node_id }), h('td', { class: 'mono', text: p.new_file + '\n' + p.new_node_id }),
            h('td', { text: (p.confidence * 100).toFixed(0) + '%' }), h('td', { text: p.reason })))))),
        after ? h('div', { class: 'muted', text: 'Grounding score after apply: ' + after.score.toFixed(1) + '%' }) : null];
}

export async function page(ctx) {
    const { main } = ctx;
    const d = await api('/api/drift');
    const g = d.grounding || {};
    const out = h('div', { 'aria-live': 'polite' }, empty('Run a preview to see proposed relocations.'));
    let preview = null;
    const applyBtn = h('button', { type: 'button', class: 'btn btn-danger', text: 'Apply relocations…', disabled: true, onclick: async () => {
        const n = preview ? preview.proposals.length : 0;
        if (!await confirmDialog('Apply relocations?', 'Apply ' + n + ' grounding relocation(s)? This rewrites the affected scaffold Markdown files.', { okText: 'Apply', danger: true })) return;
        applyBtn.disabled = true;
        try {
            const res = await post('/api/drift/sync', { dryRun: false });
            fill(out, results(res, res.driftAfter));
            toast(res.result.message, 'success');
            preview = null;
        } catch (err) { fill(out, h('div', { class: 'notice notice-error', text: err.message })); applyBtn.disabled = false; }
    } });
    const previewBtn = h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview relocations', onclick: async () => {
        previewBtn.disabled = true;
        fill(out, h('div', { class: 'loading', text: 'Scanning…' }));
        try {
            const res = await post('/api/drift/sync', { dryRun: true });
            preview = res.result;
            fill(out, results(res));
            applyBtn.disabled = !res.result.proposals.length;
        } catch (err) { fill(out, h('div', { class: 'notice notice-error', text: err.message })); }
        finally { previewBtn.disabled = false; }
    } });
    fill(main,
        pageHeader('Groundings', 'Detect code anchors that moved and relocate them. Preview is a dry run; Apply rewrites the affected scaffold files.',
            h('button', { type: 'button', class: 'btn', text: 'Run drift check job', onclick: () => startJob('drift_check', 'Drift check') })),
        h('div', { class: 'kpis' },
            kpi(d.score.toFixed(1) + '%', 'Drift score', d.score >= 95 ? 'val-success' : d.score >= 80 ? 'val-warning' : 'val-danger'),
            kpi(fmtNum(g.intact), 'Intact anchors', 'val-success'),
            kpi(fmtNum(g.changed), 'Changed anchors', g.changed ? 'val-warning' : ''),
            kpi(fmtNum(g.missing), 'Missing anchors', g.missing ? 'val-danger' : ''),
            kpi(fmtNum(d.issue_count), 'Drift issues')),
        card('Relocations', [h('div', { class: 'action-row' }, previewBtn, applyBtn), out]),
        card('Drift issues', d.issues.length ? h('div', { class: 'table-wrap' }, h('table', { class: 'data' },
            h('thead', null, h('tr', null, ['Severity', 'File', 'Symbol', 'Message'].map(t => h('th', { scope: 'col', text: t })))),
            h('tbody', null, d.issues.map(i => h('tr', null, h('td', null, badge(i.severity, i.severity === 'error' ? 'danger' : 'warning')),
                h('td', { class: 'mono', text: i.file }), h('td', { class: 'mono', text: i.symbol || '—' }), h('td', { text: i.message }))))))
            : empty('No drift issues. Scaffold and code agree.')));
}
