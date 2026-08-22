// Knowledge entity page (/knowledge/:id): groundings with source, the drift
// panel (committed baseline vs current source, drift codes, open symbol and
// sync preview), the supersession timeline, the evidence panel (sources,
// provenance, grounding health, traceability), relations, backlinks, related
// entities, body, and "propose an update" authoring.

import { h, fill, api, post, pageHeader, card, badge, statusBadge, link, enc, codeLines, empty, diffView, kv, fmtTime, openDialog, toast } from '../core.js';
import { editDraft } from './inbox.js';

const HEALTH_KIND = { fresh: 'success', unverified: 'neutral', ambiguous: 'warning', changed: 'warning', missing: 'danger' };
const healthBadge = hl => hl ? badge(hl, HEALTH_KIND[hl] || 'neutral') : badge('not checked', 'neutral');
const severityKind = s => s === 'error' ? 'danger' : s === 'warning' ? 'warning' : 'neutral';

function entityLink(e) {
    return h('li', null, link('/knowledge/' + enc(e.id), e.title), h('span', { class: 'muted small', text: ' · ' + (e.type || e.entity_type) + (e.summary ? ' — ' + e.summary : '') }));
}

/// Tolerant fetch: a panel that fails to load never breaks the page.
async function optional(path) {
    try { return await api(path); } catch (err) { return { error: err }; }
}

// -- Drift panel -------------------------------------------------------------

function sourceSide(title, side, missingText) {
    return h('div', { class: 'drift-side' },
        h('div', { class: 'drift-side-head' }, h('strong', { text: title }),
            side.bodyHash ? h('span', { class: 'mono small muted', text: ' ' + String(side.bodyHash).slice(0, 12) }) : null),
        side.source != null ? codeLines(String(side.source).split('\n'), 1) : h('div', { class: 'muted small', text: missingText }),
        side.truncated ? h('div', { class: 'muted small', text: 'Truncated.' }) : null);
}

async function syncPreview(refs) {
    let res;
    try { res = await post('/api/drift/sync', { dryRun: true }); } catch (err) { toast(err.message, 'error'); return; }
    const r = res.result || {};
    const mine = (r.proposals || []).filter(p => refs.includes(p.old_node_id));
    await openDialog('Sync preview (dry run)', close => [
        h('p', { class: 'dialog-text', text: r.skipped || r.message || '' }),
        mine.length ? h('ul', { class: 'plain' }, mine.map(p => h('li', null,
            h('span', { class: 'mono', text: p.old_node_id }), ' → ', h('span', { class: 'mono', text: p.new_node_id }),
            h('span', { class: 'muted small', text: ' · ' + Math.round((p.confidence || 0) * 100) + '% · ' + p.reason }))))
            : empty('No relocation is proposed for this entity\'s groundings.'),
        (r.proposals || []).length > mine.length ? h('p', { class: 'muted small', text: ((r.proposals.length - mine.length) + ' other proposal(s) in the scaffold.') }) : null,
        h('div', { class: 'dialog-actions' }, link('/groundings', 'Review all relocations', 'btn'),
            h('button', { type: 'button', class: 'btn btn-primary', text: 'Close', onclick: () => close(null) })),
    ], { wide: true });
}

function driftPane(p) {
    const showDiff = h('button', { type: 'button', class: 'btn btn-sm', text: 'Show diff', 'aria-pressed': 'false' });
    const sides = h('div', { class: 'drift-sides' },
        sourceSide('Committed baseline (old)', p.baseline,
            p.baseline.sourceNote || (p.baseline.committed ? 'Only the committed body hash is known; the old source is not available locally. The current source is shown alongside.' : 'No baseline has been recorded for this grounding.')),
        sourceSide('Current source (new)', p.current, p.resolved ? 'Source not available in this checkout.' : 'The grounded symbol no longer resolves in the code graph.'));
    const diff = p.diff ? diffView(p.diff) : null;
    let diffShown = false;
    const body = h('div', { class: 'drift-body' }, sides);
    showDiff.onclick = () => {
        diffShown = !diffShown;
        showDiff.textContent = diffShown ? 'Side by side' : 'Show diff';
        showDiff.setAttribute('aria-pressed', String(diffShown));
        fill(body, diffShown && diff ? diff : sides);
    };
    return h('article', { class: 'grounding drift-pane' + (p.drifted ? ' drifted' : '') },
        h('div', { class: 'grounding-head' },
            p.symbol ? link('/code/symbols/' + enc(p.symbol.id), p.symbol.qualifiedName || p.symbol.name, 'grounding-name') : h('span', { class: 'mono', text: p.ref }),
            healthBadge(p.health),
            (p.issues || []).map(i => h('span', { class: 'badge badge-' + severityKind(i.severity), title: i.message, text: i.code })),
            p.symbol ? h('span', { class: 'mono small muted', text: p.symbol.filePath + ':' + p.symbol.startLine + '-' + p.symbol.endLine }) : null),
        (p.issues || []).length ? h('ul', { class: 'plain small' }, p.issues.map(i => h('li', null, h('strong', { text: i.code + ': ' }), i.message,
            i.candidate ? h('span', { class: 'mono muted', text: ' (candidate ' + i.candidate + ')' }) : null))) : null,
        body,
        h('div', { class: 'action-row' },
            p.symbol ? link('/code/symbols/' + enc(p.symbol.id), 'Open symbol', 'btn btn-sm') : null,
            diff ? showDiff : null,
            h('button', { type: 'button', class: 'btn btn-sm', text: 'Run sync preview', onclick: () => syncPreview([p.ref]) })));
}

export function driftPanel(d) {
    if (!d || d.error) return d && d.error ? card('Drift', h('div', { class: 'muted', text: d.error.message })) : null;
    const drifted = (d.panes || []).filter(p => p.drifted || (p.issues || []).length);
    if (!drifted.length && !d.unavailable) return null;
    const aside = h('span', null, (d.codes || []).map(c => badge(c, 'warning')));
    return card('Drift (' + drifted.length + ')', [
        h('p', { class: 'muted small', text: d.graph ? d.graph.summary : '' }),
        d.unavailable ? h('div', { class: 'notice notice-warning', text: 'The code graph is not built, so nothing could be compared. This is not the same as "no drift".' }) : null,
        d.graphStale && !d.unavailable ? h('div', { class: 'notice notice-info', text: 'The graph is not fresh; current source is read at the recorded line span.' }) : null,
        drifted.map(driftPane),
        drifted.length > 1 ? h('div', { class: 'action-row' }, h('button', { type: 'button', class: 'btn', text: 'Sync preview for all', onclick: () => syncPreview(drifted.map(p => p.ref)) })) : null,
    ], { aside });
}

// -- Supersession timeline ---------------------------------------------------

export function timelinePanel(t) {
    if (!t || t.error || !(t.entries || []).length) return null;
    if (t.entries.length < 2 && !(t.cycles || []).length) return null;
    return card('Supersession timeline', [
        (t.cycles || []).length ? h('div', { class: 'notice notice-error', text: 'Supersession cycle: ' + t.cycles.map(c => c.join(' → ')).join('; ') }) : null,
        h('ol', { class: 'timeline', 'aria-label': 'Supersession chain, oldest first' }, t.entries.map(e => h('li', { class: 'timeline-entry' + (e.current ? ' current' : '') + (e.origin ? ' origin' : ''), 'aria-current': e.origin ? 'true' : null },
            h('div', { class: 'timeline-head' },
                e.origin ? h('strong', { text: e.entity.title }) : link('/knowledge/' + enc(e.entity.id), e.entity.title),
                ' ', statusBadge(e.lifecycle), e.current ? badge('current', 'accent') : null),
            h('div', { class: 'muted small' },
                [e.date ? fmtTime(e.date) : 'no date', 'rev ' + e.revision,
                    e.supersedes ? 'supersedes ' + e.supersedes : null, e.supersededBy ? 'superseded by ' + e.supersededBy : null].filter(Boolean).join(' · '))))),
        t.truncated ? h('div', { class: 'muted small', text: 'The chain is longer than shown.' }) : null,
    ]);
}

// -- Evidence panel ----------------------------------------------------------

function traceView(tr) {
    if (!tr) return empty('No traceability (the entity is not in a spec chain).');
    const chain = Object.entries(tr.nodes || {}).map(([type, nodes]) => h('li', null, h('strong', { text: type + ': ' }),
        nodes.map((n, i) => [i ? ', ' : '', link('/knowledge/' + enc(n.entity.id), n.entity.title)])));
    return h('div', null,
        chain.length ? h('ul', { class: 'plain' }, chain) : null,
        (tr.implementations || []).length ? h('div', { class: 'small' }, h('strong', { text: 'Implementations: ' }), tr.implementations.map(([, ref, hl], i) => [i ? ', ' : '', h('span', { class: 'mono', text: ref }), hl ? ' ' : '', hl ? healthBadge(hl) : null])) : null,
        (tr.tests || []).length ? h('div', { class: 'small' }, h('strong', { text: 'Tests: ' }), tr.tests.map(([, , file], i) => [i ? ', ' : '', h('span', { class: 'mono', text: file })])) : null,
        (tr.acceptanceCriteria || []).length ? h('div', { class: 'small' }, h('strong', { text: 'Acceptance criteria: ' }), tr.acceptanceCriteria.map(([, id, title], i) => [i ? ', ' : '', link('/knowledge/' + enc(id), title)])) : null,
        (tr.gaps || []).length ? h('ul', { class: 'plain small' }, tr.gaps.map(g => h('li', null, badge('gap', 'warning'), ' ', h('span', { class: 'mono', text: g.hop }), ' — ' + g.reason))) : null);
}

export function evidencePanel(ev) {
    if (!ev || ev.error) return null;
    const p = ev.provenance;
    return card('Evidence', [
        h('h3', { class: 'card-sub', text: 'Sources' }),
        (ev.sources || []).length ? h('ul', { class: 'plain' }, ev.sources.map(s => h('li', null, badge(s.type),
            ' ', s.ref ? (s.type === 'url' && /^https?:\/\//.test(s.ref) ? h('a', { href: s.ref, rel: 'noopener noreferrer', target: '_blank', text: s.ref }) : h('span', { class: 'mono', text: s.ref })) : null,
            s.note ? h('span', { class: 'muted small', text: ' — ' + s.note }) : null,
            s.captured_at ? h('span', { class: 'muted small', text: ' · ' + fmtTime(s.captured_at) }) : null)))
            : empty('No sources declared.'),
        h('h3', { class: 'card-sub', text: 'Provenance' }),
        p ? kv([['Created by', (p.created_by || {}).kind + ':' + (p.created_by || {}).id], ['Created', fmtTime(p.created_at)],
            ['Last modified by', p.last_modified_by ? p.last_modified_by.kind + ':' + p.last_modified_by.id : '—'], ['Last modified', fmtTime(p.last_modified_at)],
            ['Agent session', p.agent_session_id || '—']]) : empty('No provenance recorded.'),
        h('h3', { class: 'card-sub' }, 'Grounding health ', healthBadge(ev.health)),
        (ev.groundings || []).length ? h('ul', { class: 'plain' }, ev.groundings.map(g => h('li', null, h('span', { class: 'mono', text: g.ref }), ' ', healthBadge(g.health),
            h('span', { class: 'muted small', text: ' · ' + g.origin + (g.committedBaseline ? ' · committed baseline' : ' · no committed baseline') }))))
            : empty('No groundings.'),
        h('h3', { class: 'card-sub', text: 'Traceability' }),
        traceView(ev.traceability),
    ]);
}

export async function page(ctx) {
    const id = ctx.params.id;
    const [d, drift, timeline, evidence] = await Promise.all([
        api('/api/wiki/entity?id=' + enc(id)),
        optional('/api/wiki/entity/drift?id=' + enc(id)),
        optional('/api/wiki/entity/timeline?id=' + enc(id)),
        optional('/api/wiki/entity/evidence?id=' + enc(id)),
    ]);
    if (!ctx.isCurrent()) return;
    const e = d.entity;
    const isSpec = String(e.file || '').startsWith('specs/');
    const groundings = d.groundings.map(gr => gr.resolved
        ? h('article', { class: 'grounding' },
            h('div', { class: 'grounding-head' },
                link('/code/symbols/' + enc(gr.resolvedId || gr.nodeId), gr.node.qualified_name || gr.node.name, 'grounding-name'),
                badge(gr.node.kind, 'success'), gr.health ? healthBadge(gr.health) : null, h('span', { class: 'mono small muted', text: gr.node.file_path + ':' + gr.node.start_line + '-' + gr.node.end_line })),
            gr.snippet ? codeLines(String(gr.snippet.code).split('\n'), gr.snippet.startLine) : h('div', { class: 'muted', text: 'Source not available in this checkout.' }),
            gr.snippet && gr.snippet.truncated ? h('div', { class: 'muted small', text: 'Truncated; open the symbol page for the full source.' }) : null)
        : h('article', { class: 'grounding missing' },
            h('div', { class: 'grounding-head' }, h('span', { class: 'mono', text: gr.nodeId }), badge('unresolved', 'danger')),
            h('div', { class: 'muted' }, d.graphAvailable ? 'This anchor no longer resolves to a code node. ' : 'The code graph is not built. ', link('/groundings', 'Preview relocations'))));
    fill(ctx.main,
        link('/context', '← Context', 'back-link'),
        pageHeader(e.title, h('span', { class: 'detail-meta' }, badge(e.entity_type, 'accent'), statusBadge(e.status), h('span', { class: 'mono small', text: e.file }), h('span', { class: 'muted small', text: 'rev ' + e.revision })),
            h('button', { type: 'button', class: 'btn btn-primary', text: 'Propose an update', onclick: () => editDraft(null, {
                change: { kind: isSpec ? 'spec.update' : 'knowledge.update', target: { id: e.id, title: e.title }, patch: { title: e.title, summary: e.summary || '', body: e.body || '' } },
            }) })),
        e.summary ? h('p', { class: 'lede', text: e.summary }) : null,
        (e.topics || []).length ? h('div', { class: 'chips' }, e.topics.map(t => badge(t))) : null,
        driftPanel(drift),
        h('div', { class: 'two-col' },
            h('div', null,
                card('Groundings (' + d.groundings.length + ')', groundings.length ? groundings : empty('No code groundings declared.')),
                timelinePanel(timeline),
                card('Body', h('pre', { class: 'body', text: e.body || '' }))),
            h('div', null,
                evidencePanel(evidence),
                card('Relations', e.relations.length ? h('ul', { class: 'plain' }, e.relations.map(r => h('li', null, h('span', { class: 'muted', text: r.type + ' → ' }),
                    link('/knowledge/' + enc(r.target_id), r.target_id), r.note ? h('span', { class: 'muted small', text: ' — ' + r.note }) : null))) : empty('None.')),
                card('Referenced by', d.backlinks.length ? h('ul', { class: 'plain' }, d.backlinks.map(entityLink)) : empty('Nothing links here.')),
                card('Related', d.related.length ? h('ul', { class: 'plain' }, d.related.map(entityLink)) : empty('No related entities.')))));
}
