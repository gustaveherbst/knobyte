// Inbox: review grouping, proposal detail with diff and evidence, review
// decisions (approve / reject / withdraw / mark stale / repair) and draft
// authoring (knowledge / spec create & update, or a scaffold file edit),
// all through preview → apply.

import { h, fill, api, pageHeader, card, empty, badge, statusBadge, fmtTime, link, enc, diffView, openDialog, field, lines, app, navigate, render } from '../core.js';
import { runOperation, parseEvidence, evidenceText } from '../team.js';

const KNOWLEDGE_KINDS = ['architecture', 'component', 'convention', 'decision', 'pattern', 'guide'];
const SPEC_KINDS = ['spec', 'requirement', 'constraint', 'acceptance_criterion'];
const SPEC_RELATIONS = ['derived_from', 'refines', 'constrained_by', 'verified_by'];

function me() { return app.shell && app.shell.actor && app.shell.actor.member ? app.shell.actor.member.id : null; }

function changeLabel(c) {
    if (!c) return 'file edit';
    return c.kind.replace('.', ' ');
}

function proposalRow(p, selected) {
    return h('a', { href: '/inbox/' + enc(p.id), 'data-link': '', class: 'list-item' + (selected ? ' selected' : ''), 'aria-current': selected ? 'true' : null },
        h('div', { class: 'list-item-title', text: p.title }),
        h('div', { class: 'list-item-meta' }, statusBadge(p.status), badge(changeLabel(p.change), 'neutral'),
            h('span', { class: 'mono', text: p.target }), h('span', { text: '@' + p.author }), h('span', { text: fmtTime(p.updatedAt) })));
}

const contributes = (p, mine) => !!mine && (p.author === mine || (p.repairedBy || []).includes(mine));

function groups(proposals) {
    const mine = me();
    return [
        ['Needs your review', proposals.filter(p => p.status === 'pending' && !contributes(p, mine))],
        ['Your open proposals', proposals.filter(p => p.status === 'pending' && contributes(p, mine))],
        ['Stale: needs repair', proposals.filter(p => p.status === 'stale')],
        ['Decided', proposals.filter(p => !['pending', 'stale'].includes(p.status))],
    ];
}

export async function page(ctx) {
    const { main, params } = ctx;
    const data = await api('/api/inbox');
    const filter = ctx.query.get('show') || 'open';
    const list = h('div', { class: 'card list-panel' });
    const visible = groups(data.proposals).filter(([name]) => filter === 'all' || name !== 'Decided');
    fill(list, visible.map(([name, items]) => items.length ? h('section', { class: 'list-group', 'aria-label': name },
        h('h2', { class: 'list-group-title', text: name + ' (' + items.length + ')' }), items.map(p => proposalRow(p, p.id === params.id))) : null));
    if (!list.childElementCount) fill(list, empty('No open proposals. Agents and teammates submit them as drafts; publish a draft to open review.'));
    const detail = h('div', { class: 'card detail-panel', 'aria-live': 'polite' });
    if (params.id) await showProposal(detail, params.id, () => render());
    else fill(detail, empty('Select a proposal to review.'));

    fill(main,
        pageHeader('Inbox', 'Review proposed knowledge before it lands in .knobyte/. Every change is previewed before it is applied.',
            h('button', { type: 'button', class: 'btn btn-primary', text: 'New draft', onclick: () => editDraft(null, {}) })),
        !data.currentMember ? h('div', { class: 'notice notice-warning' }, 'No member is selected for this checkout, so reviews cannot be attributed. ', link('/members', 'Choose who you are working as')) : null,
        h('div', { class: 'chip-row', role: 'group', 'aria-label': 'Show' },
            ['open', 'all'].map(f => h('a', { href: '/inbox' + (params.id ? '/' + enc(params.id) : '') + (f === 'all' ? '?show=all' : ''), 'data-link': '', class: 'chip' + (filter === f ? ' active' : ''), 'aria-pressed': String(filter === f), text: f === 'open' ? 'Open' : 'Include decided' }))),
        h('div', { class: 'split' }, list, detail),
        card('Local drafts (' + data.drafts.length + ')', data.drafts.length ? h('ul', { class: 'row-list' }, data.drafts.map(d => draftRow(d))) : empty('No local drafts. Drafts stay in this checkout until you publish them.')));
}

function draftRow(d) {
    return h('li', { class: 'row-item' },
        h('div', { class: 'grow' },
            h('div', { class: 'list-item-title', text: d.title || '(untitled draft)' }),
            h('div', { class: 'list-item-meta' }, badge(changeLabel(d.change), 'neutral'), h('span', { class: 'mono', text: d.target }), h('span', { text: '@' + d.author }), h('span', { text: fmtTime(d.updatedAt || d.createdAt) })),
            d.reason ? h('div', { class: 'muted small', text: d.reason }) : null),
        h('div', { class: 'row-actions' },
            h('button', { type: 'button', class: 'btn btn-sm', text: 'Edit', onclick: () => editDraft(d, {}) }),
            h('button', { type: 'button', class: 'btn btn-sm', text: 'Delete', onclick: async () => {
                if (await runOperation({ kind: 'inbox.draft.delete', draftId: d.id }, { title: 'Delete draft "' + (d.title || d.id) + '"', okText: 'Delete', danger: true })) render();
            } }),
            h('button', { type: 'button', class: 'btn btn-sm btn-primary', text: 'Publish', onclick: async () => {
                const r = await runOperation({ kind: 'inbox.publish', draftId: d.id }, { title: 'Publish draft for review', okText: 'Publish' });
                if (r && r.result && r.result.id) navigate('/inbox/' + enc(r.result.id)); else if (r) render();
            } })));
}

function evidenceList(evidence) {
    if (!evidence || !evidence.length) return h('div', { class: 'muted', text: 'No evidence attached.' });
    return h('ul', { class: 'plain evidence' }, evidence.map(e => h('li', null, badge(e.kind, 'neutral'), ' ',
        e.kind === 'code' ? link('/code/symbols/' + enc(e.symbolId), e.symbolId)
            : e.kind === 'entity' ? link('/knowledge/' + enc(e.id), e.id)
                : e.kind === 'external' ? h('a', { href: e.uri, rel: 'noopener noreferrer', target: '_blank', text: e.label || e.uri })
                    : h('span', { class: e.kind === 'manual' ? '' : 'mono', text: evidenceText(e) }))));
}

async function decide(p, kind, after) {
    const mine = me();
    const isAuthor = mine && (p.author === mine || (p.repairedBy || []).includes(mine));
    const needsReason = kind === 'inbox.mark-stale';
    const labels = { 'inbox.approve': 'Approve', 'inbox.reject': 'Reject', 'inbox.withdraw': 'Withdraw', 'inbox.mark-stale': 'Mark stale' };
    const input = await openDialog(labels[kind] + ': ' + p.title, close => {
        const reason = h('textarea', { class: 'input', maxlength: 4000, autofocus: true, 'aria-label': 'Rationale' });
        const selfBox = h('input', { type: 'checkbox', id: 'self-approve' });
        return [
            field(needsReason ? 'Why is it stale? (required)' : 'Rationale (optional)', reason),
            kind === 'inbox.approve' && isAuthor ? h('label', { class: 'check', for: 'self-approve' }, selfBox, ' Approve my own proposal without teammate review (recorded as self-approved)') : null,
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview', onclick: () => {
                    if (needsReason && !reason.value.trim()) { reason.focus(); return; }
                    close({ rationale: reason.value.trim() || undefined, selfApprove: selfBox.checked || undefined });
                } })),
        ];
    });
    if (!input) return;
    const action = Object.assign({ kind, proposalId: p.id }, input);
    const hint = err => (kind === 'inbox.approve' && err && err.code === 'SELF_APPROVAL_REQUIRED')
        ? 'You authored this proposal. Ask a teammate to review it, or choose Self-approve and tick "Approve my own proposal without teammate review".'
        : null;
    if (await runOperation(action, { title: labels[kind] + ' proposal', okText: labels[kind], danger: kind !== 'inbox.approve', hint })) after();
}

async function showProposal(panel, id, after) {
    let d;
    try { d = await api('/api/inbox/' + enc(id)); } catch (err) { fill(panel, h('div', { class: 'notice notice-error', text: err.message })); return; }
    const p = d.proposal;
    const mine = me();
    const isAuthor = mine && (p.author === mine || (p.repairedBy || []).includes(mine));
    const pending = p.status === 'pending';
    fill(panel,
        h('h2', { class: 'detail-title', text: p.title }),
        h('div', { class: 'detail-meta' }, statusBadge(p.status), badge(changeLabel(p.change), 'accent'), badge(p.mode, 'neutral'),
            h('span', { class: 'mono small', text: '.knobyte/' + (d.target || p.target) }), h('span', { text: 'by @' + p.author }), h('span', { text: fmtTime(p.createdAt) })),
        p.reason ? h('p', { text: p.reason }) : null,
        p.decisionBy ? h('div', { class: 'notice ' + (p.status === 'approved' ? 'notice-success' : 'notice-warning') },
            p.status + ' by @' + p.decisionBy + ' at ' + fmtTime(p.decidedAt) + (p.selfApproved ? ' (self-approved)' : '') + (p.decisionReason ? ' — ' + p.decisionReason : '')) : null,
        p.staleReason ? h('div', { class: 'notice notice-warning', text: 'Stale: ' + p.staleReason }) : null,
        d.decided ? null : h('section', { class: 'detail-section' },
            h('h3', { class: 'label', text: d.targetError ? 'Target error' : (d.targetExists ? 'Diff against current ' + d.target + ' (' + p.mode + ')' : 'New file ' + d.target) }),
            d.targetError ? h('div', { class: 'notice notice-error', text: d.targetError }) : diffView(d.diff)),
        h('section', { class: 'detail-section' }, h('h3', { class: 'label', text: 'Evidence' }), evidenceList(p.evidence)),
        h('details', { class: 'detail-section' }, h('summary', { text: 'Proposed content' }), h('pre', { class: 'body', text: p.proposedContent })),
        h('div', { class: 'action-row' },
            pending && !isAuthor ? h('button', { type: 'button', class: 'btn btn-success', text: 'Approve…', disabled: !mine, onclick: () => decide(p, 'inbox.approve', after) }) : null,
            pending && isAuthor ? h('button', { type: 'button', class: 'btn', text: 'Self-approve…', onclick: () => decide(p, 'inbox.approve', after) }) : null,
            pending && !isAuthor ? h('button', { type: 'button', class: 'btn btn-danger', text: 'Reject…', disabled: !mine, onclick: () => decide(p, 'inbox.reject', after) }) : null,
            pending && isAuthor ? h('button', { type: 'button', class: 'btn btn-danger', text: 'Withdraw…', onclick: () => decide(p, 'inbox.withdraw', after) }) : null,
            pending ? h('button', { type: 'button', class: 'btn', text: 'Mark stale…', disabled: !mine, onclick: () => decide(p, 'inbox.mark-stale', after) }) : null,
            p.status === 'stale' ? h('button', { type: 'button', class: 'btn btn-primary', text: 'Repair…', onclick: () => editDraft(null, { repair: p }) }) : null,
            mine ? h('span', { class: 'muted small', text: 'as @' + mine }) : h('span', { class: 'muted small', text: 'Select a member to review.' })));
}

// ---------------------------------------------------------------------------
// Draft editor
// ---------------------------------------------------------------------------

function select(id, options, value) {
    return h('select', { class: 'input', id }, options.map(o => h('option', { value: o, selected: o === value ? true : null, text: o.replace(/_/g, ' ') })));
}

/// Open the draft editor. `draft`: existing draft to edit (or null);
/// opts.change: preset typed change; opts.repair: stale proposal to repair.
export async function editDraft(draft, opts) {
    opts = opts || {};
    const src = opts.repair || draft || {};
    const preset = opts.change || src.change || null;
    const initialType = preset ? preset.kind : (src.target ? 'file' : 'knowledge.create');
    const result = await openDialog(opts.repair ? 'Repair stale proposal' : draft ? 'Edit draft' : 'New draft', close => {
        const type = select('draft-type', ['knowledge.create', 'knowledge.update', 'spec.create', 'spec.update', 'file'], initialType);
        const p = preset || {};
        const patch = p.patch || {};
        const title = h('input', { class: 'input', id: 'draft-title', maxlength: 512, value: p.title || patch.title || src.title || '' });
        const entityKind = h('select', { class: 'input', id: 'draft-entity-kind' });
        const summary = h('input', { class: 'input', id: 'draft-summary', maxlength: 1024, value: p.summary || patch.summary || '' });
        const body = h('textarea', { class: 'input tall', id: 'draft-body', value: p.body || patch.body || (initialType === 'file' ? src.proposedContent || '' : '') });
        const topics = h('input', { class: 'input', id: 'draft-topics', value: (p.topics || []).join(', '), placeholder: 'comma separated' });
        const targetId = h('input', { class: 'input', id: 'draft-target-id', value: (p.target && p.target.id) || '' , placeholder: 'entity id' });
        const relType = select('draft-rel-type', ['', ...SPEC_RELATIONS], (p.relation && p.relation.type) || '');
        const relTarget = h('input', { class: 'input', id: 'draft-rel-target', value: (p.relation && p.relation.target && p.relation.target.id) || '', placeholder: 'related entity id' });
        const filePath = h('input', { class: 'input', id: 'draft-file', value: src.target || '', placeholder: 'context/architecture.md (relative to .knobyte/)' });
        const mode = select('draft-mode', ['replace', 'append'], src.mode || 'replace');
        const rationale = h('textarea', { class: 'input', id: 'draft-rationale', maxlength: 4000, value: src.reason || '' });
        const evidence = h('textarea', { class: 'input', id: 'draft-evidence', value: (src.evidence || []).map(evidenceText).join('\n'),
            placeholder: 'one per line: code:<symbol>, entity:<id>, file:<path>, commit:<hash>, https://…, or a note' });
        const err = h('div', { role: 'alert' });
        const sections = {
            create: h('div', { class: 'field-group' }, h('div', { class: 'field-row' }, field('Kind', entityKind), field('Topics', topics))),
            update: h('div', { class: 'field-group' }, field('Target entity id', targetId, 'The entity whose title, summary or body you propose to change.')),
            relation: h('div', { class: 'field-group field-row' }, field('Relation (optional)', relType), field('Related entity', relTarget)),
            content: h('div', { class: 'field-group' }, field('Summary', summary), field('Body (Markdown)', body)),
            file: h('div', { class: 'field-group field-row' }, field('Scaffold file', filePath), field('Mode', mode)),
        };
        const sync = () => {
            const t = type.value;
            const kinds = t.startsWith('spec') ? SPEC_KINDS : KNOWLEDGE_KINDS;
            const cur = entityKind.value || p.entityKind;
            fill(entityKind, kinds.map(k => h('option', { value: k, selected: k === cur ? true : null, text: k.replace(/_/g, ' ') })));
            sections.create.hidden = !t.endsWith('.create');
            sections.update.hidden = !t.endsWith('.update');
            sections.relation.hidden = t !== 'spec.create';
            sections.file.hidden = t !== 'file';
            summary.closest('.field').hidden = t === 'file';
        };
        type.addEventListener('change', sync);
        setTimeout(sync, 0);
        const build = () => {
            const t = type.value;
            const input = { title: title.value.trim() || undefined, rationale: rationale.value.trim(), evidence: lines(evidence.value).map(parseEvidence) };
            if (!input.rationale) throw new Error('A rationale is required: say why this change is right.');
            if (t === 'file') {
                if (!filePath.value.trim()) throw new Error('Choose the scaffold file to change.');
                Object.assign(input, { target: filePath.value.trim(), content: body.value, mode: mode.value });
            } else if (t.endsWith('.create')) {
                if (!title.value.trim() || !body.value.trim()) throw new Error('A title and a body are required.');
                const change = { kind: t, entityKind: entityKind.value, title: title.value.trim(), body: body.value };
                if (summary.value.trim()) change.summary = summary.value.trim();
                const tp = topics.value.split(',').map(x => x.trim()).filter(Boolean);
                if (tp.length) change.topics = tp;
                if (t === 'spec.create' && relType.value && relTarget.value.trim()) change.relation = { type: relType.value, target: { id: relTarget.value.trim() } };
                input.change = change;
            } else {
                if (!targetId.value.trim()) throw new Error('Name the entity to update.');
                const pt = {};
                if (title.value.trim()) pt.title = title.value.trim();
                if (summary.value.trim()) pt.summary = summary.value.trim();
                if (body.value.trim()) pt.body = body.value;
                input.change = { kind: t, target: { id: targetId.value.trim() }, patch: pt };
            }
            return input;
        };
        return [
            field('Type', type),
            field('Title', title),
            sections.create, sections.update, sections.relation, sections.file, sections.content,
            field('Rationale (required)', rationale),
            field('Evidence', evidence),
            err,
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-primary', text: opts.repair ? 'Preview repair' : 'Preview save', onclick: () => {
                    try { close(build()); } catch (e) { fill(err, h('div', { class: 'notice notice-error', text: e.message })); }
                } })),
        ];
    }, { wide: true, sticky: true });
    if (!result) return null;
    const action = opts.repair
        ? { kind: 'inbox.repair', proposalId: opts.repair.id, replacement: result }
        : Object.assign({ kind: 'inbox.draft.save', draft: result }, draft ? { draftId: draft.id } : {});
    const res = await runOperation(action, { title: opts.repair ? 'Repair proposal' : 'Save local draft', okText: opts.repair ? 'Repair' : 'Save draft' });
    if (res) {
        if (opts.repair) navigate('/inbox/' + enc(opts.repair.id));
        else if (window.location.pathname.startsWith('/inbox')) render();
        else navigate('/inbox');
    }
    return res;
}
