// Relays: perspective lists, detail sections, draft composer with a
// recipients picker, publish, acknowledge and close (preview → apply).

import { h, fill, api, pageHeader, card, empty, badge, statusBadge, fmtTime, link, enc, kv, openDialog, field, lines, app, navigate, render } from '../core.js';
import { runOperation, parseEvidence, evidenceText, parseCodeRef, codeRefText } from '../team.js';

function me() { return app.shell && app.shell.actor && app.shell.actor.member ? app.shell.actor.member.id : null; }
function recipientsText(r) { return r.openToTeam || r.audience === 'team' ? 'open to the team' : 'to ' + (r.namedRecipients || []).map(x => '@' + x).join(', '); }

export async function page(ctx) {
    const { main, params } = ctx;
    const perspective = ctx.query.get('view') || (me() ? 'mine' : 'all');
    const [data, pageData] = await Promise.all([
        api('/api/relays'),
        api('/api/relays/page?perspective=' + enc(perspective) + '&limit=100').catch(() => null),
    ]);
    const relays = pageData ? pageData.items : data.relays;
    const list = h('div', { class: 'card list-panel' },
        relays.length ? relays.map(r => h('a', { href: '/relays/' + enc(r.id) + (ctx.query.get('view') ? '?view=' + enc(perspective) : ''), 'data-link': '', class: 'list-item' + (r.id === params.id ? ' selected' : ''), 'aria-current': r.id === params.id ? 'true' : null },
            h('div', { class: 'list-item-title', text: r.title }),
            h('div', { class: 'list-item-meta' }, statusBadge(r.status), h('span', { text: 'from @' + r.sender }), h('span', { text: recipientsText(r) }),
                r.claimant ? h('span', { text: 'taken by @' + r.claimant }) : null, h('span', { text: fmtTime(r.updatedAt) }))))
            : empty(perspective === 'mine' ? 'No handoffs are waiting for you.' : 'No relays yet.'));
    const detail = h('div', { class: 'card detail-panel', 'aria-live': 'polite' });
    if (params.id) await showRelay(detail, params.id);
    else fill(detail, empty('Select a relay.'));
    fill(main,
        pageHeader('Relays', 'Structured handoffs between engineers. Acknowledge to take one; the sender or claimant closes it.',
            h('button', { type: 'button', class: 'btn btn-primary', text: 'New relay draft', onclick: () => composeDraft(null) })),
        !me() ? h('div', { class: 'notice notice-warning' }, 'No member is selected for this checkout. ', link('/members', 'Choose who you are working as'), ' to acknowledge or close relays.') : null,
        h('div', { class: 'chip-row', role: 'group', 'aria-label': 'Perspective' },
            [['mine', 'For me'], ['sent', 'Sent'], ['all', 'All']].map(([v, l]) => h('a', { href: '/relays?view=' + v, 'data-link': '', class: 'chip' + (perspective === v ? ' active' : ''), 'aria-pressed': String(perspective === v), text: l }))),
        h('div', { class: 'split' }, list, detail),
        card('Local relay drafts (' + data.drafts.length + ')', data.drafts.length ? h('ul', { class: 'row-list' }, data.drafts.map(draftRow)) : empty('No local drafts. Compose one, or let an agent prepare it.')));
}

function draftRow(d) {
    return h('li', { class: 'row-item' },
        h('div', { class: 'grow' },
            h('div', { class: 'list-item-title', text: d.title }),
            h('div', { class: 'list-item-meta' }, h('span', { text: 'from @' + d.sender }), h('span', { text: recipientsText(d) }), h('span', { text: fmtTime(d.updatedAt || d.createdAt) })),
            h('div', { class: 'muted small', text: d.summary })),
        h('div', { class: 'row-actions' },
            h('button', { type: 'button', class: 'btn btn-sm', text: 'Edit', onclick: () => composeDraft(d) }),
            h('button', { type: 'button', class: 'btn btn-sm', text: 'Delete', onclick: async () => {
                if (await runOperation({ kind: 'relay.draft.delete', draftId: d.id }, { title: 'Delete relay draft', okText: 'Delete', danger: true })) render();
            } }),
            h('button', { type: 'button', class: 'btn btn-sm btn-primary', text: 'Publish', onclick: async () => {
                const r = await runOperation({ kind: 'relay.publish', draftId: d.id }, { title: 'Publish relay "' + d.title + '"', okText: 'Publish' });
                if (r && r.result && r.result.id) navigate('/relays/' + enc(r.result.id)); else if (r) render();
            } })));
}

function section(label, items, render_) {
    if (!items || !items.length) return null;
    return h('section', { class: 'detail-section' }, h('h3', { class: 'label', text: label }), h('ul', { class: 'plain' }, items.map(i => h('li', null, render_ ? render_(i) : i))));
}

async function showRelay(panel, id) {
    let r;
    try { r = await api('/api/relays/' + enc(id)); } catch (err) { fill(panel, h('div', { class: 'notice notice-error', text: err.message })); return; }
    const mine = me();
    const obs = r.observedState || {};
    const claimant = r.claimant || r.acknowledgedBy;
    const canAck = r.status === 'published' && mine && mine !== r.sender && (r.openToTeam || r.audience === 'team' || (r.namedRecipients || []).includes(mine));
    const canClose = r.status === 'acknowledged' && mine && (mine === r.sender || mine === claimant);
    fill(panel,
        h('h2', { class: 'detail-title', text: r.title }),
        h('div', { class: 'detail-meta' }, statusBadge(r.status), h('span', { text: 'from @' + r.sender }), h('span', { text: recipientsText(r) }),
            claimant ? h('span', { text: 'taken by @' + claimant }) : null, r.workstream ? link('/workstreams/' + enc(r.workstream), 'workstream ' + r.workstream) : null),
        h('p', { class: 'relay-summary', text: r.summary }),
        section('Completed', r.completed), section('In progress', r.inProgress), section('Decisions', r.decisions),
        section('Blockers', r.blockers), section('Unresolved questions', r.unresolvedQuestions), section('Next actions', r.nextActions),
        section('Progress notes', r.progress), section('Changed files', r.changedFiles, f => h('span', { class: 'mono', text: f })),
        section('Code', r.code, c => c.kind === 'symbol' ? link('/code/symbols/' + enc(c.symbolId), c.symbolId) : h('span', { class: 'mono', text: c.path })),
        section('Evidence', (r.evidenceRefs || []).map(evidenceText).concat(r.evidence || [])),
        h('section', { class: 'detail-section' }, h('h3', { class: 'label', text: 'Observed repository state' }),
            kv([['Branch', obs.branch], ['HEAD', obs.headCommit], ['Dirty tree', obs.dirtyTree ? 'yes' : 'no'], ['Observed', fmtTime(obs.timestamp)],
                ['Created', fmtTime(r.createdAt)], ['Acknowledged', fmtTime(r.acknowledgedAt)], ['Closed', fmtTime(r.closedAt)]])),
        h('div', { class: 'action-row' },
            r.status === 'published' ? h('button', { type: 'button', class: 'btn btn-primary', text: 'Acknowledge & take', disabled: !canAck,
                title: canAck ? null : (mine === r.sender ? 'You sent this relay.' : 'Only a named recipient (or anyone, for team relays) can take it.'),
                onclick: async () => { if (await runOperation({ kind: 'relay.acknowledge', relayId: r.id }, { title: 'Take relay "' + r.title + '"', okText: 'Acknowledge' })) render(); } }) : null,
            r.status === 'acknowledged' ? h('button', { type: 'button', class: 'btn btn-danger', text: 'Close…', disabled: !canClose,
                title: canClose ? null : 'Only the sender or the claimant can close it.',
                onclick: async () => { if (await runOperation({ kind: 'relay.close', relayId: r.id }, { title: 'Close relay "' + r.title + '"', okText: 'Close relay', danger: true })) render(); } }) : null,
            mine ? h('span', { class: 'muted small', text: 'as @' + mine }) : null));
}

/// Relay draft composer with a recipients picker.
export async function composeDraft(draft) {
    const [membersPage, wsPage] = await Promise.all([
        api('/api/team/members?active=true&limit=100').catch(() => ({ items: [] })),
        api('/api/workstreams?limit=100').catch(() => ({ items: [] })),
    ]);
    const d = draft || {};
    const input = await openDialog(draft ? 'Edit relay draft' : 'New relay draft', close => {
        const title = h('input', { class: 'input', id: 'relay-title', maxlength: 512, value: d.title || '' });
        const summary = h('textarea', { class: 'input', id: 'relay-summary', value: d.summary || '' });
        const named = new Set(d.namedRecipients || []);
        const teamAudience = d.audience ? d.audience === 'team' : (d.openToTeam !== false && !named.size);
        const audTeam = h('input', { type: 'radio', name: 'relay-aud', id: 'aud-team', value: 'team', checked: teamAudience });
        const audMembers = h('input', { type: 'radio', name: 'relay-aud', id: 'aud-members', value: 'members', checked: !teamAudience });
        const picker = h('fieldset', { class: 'recipients' }, h('legend', { class: 'field-label', text: 'Recipients' }),
            membersPage.items.filter(m => m.id !== (app.shell && app.shell.actor && app.shell.actor.member && app.shell.actor.member.id)).map(m => h('label', { class: 'check', for: 'rcp-' + m.id },
                h('input', { type: 'checkbox', id: 'rcp-' + m.id, value: m.id, checked: named.has(m.id) }), ' ', m.displayName, h('span', { class: 'muted small', text: ' @' + m.id }))));
        if (!picker.querySelector('input')) picker.appendChild(h('div', { class: 'muted small', text: 'No other active members.' }));
        const syncAud = () => { picker.disabled = audTeam.checked; };
        audTeam.addEventListener('change', syncAud); audMembers.addEventListener('change', syncAud);
        setTimeout(syncAud, 0);
        const area = (id, v, ph) => h('textarea', { class: 'input', id, value: (v || []).join('\n'), placeholder: ph || 'one per line' });
        const completed = area('relay-completed', d.completed), inProgress = area('relay-inprogress', d.inProgress), decisions = area('relay-decisions', d.decisions);
        const blockers = area('relay-blockers', d.blockers), questions = area('relay-questions', d.unresolvedQuestions), next = area('relay-next', d.nextActions);
        const files = area('relay-files', d.changedFiles, 'repository-relative paths, one per line');
        const code = area('relay-code', (d.code || []).map(codeRefText), 'symbol:<id> or file:<path>, one per line');
        const evidence = area('relay-evidence', (d.evidenceRefs || []).map(evidenceText), 'code:<symbol>, entity:<id>, commit:<hash>, URL or note');
        const ws = h('select', { class: 'input', id: 'relay-ws' }, h('option', { value: '', text: '(none)' }),
            wsPage.items.map(w => h('option', { value: w.id, selected: w.id === d.workstream ? true : null, text: w.title })));
        const err = h('div', { role: 'alert' });
        return [
            field('Title', title), field('Summary (required)', summary),
            h('fieldset', { class: 'field-row radios' }, h('legend', { class: 'field-label', text: 'Audience' }),
                h('label', { class: 'check', for: 'aud-team' }, audTeam, ' Anyone on the team'), h('label', { class: 'check', for: 'aud-members' }, audMembers, ' Named members')),
            picker,
            h('div', { class: 'field-row' }, field('Completed', completed), field('In progress', inProgress)),
            h('div', { class: 'field-row' }, field('Decisions', decisions), field('Blockers', blockers)),
            h('div', { class: 'field-row' }, field('Unresolved questions', questions), field('Next actions', next)),
            h('div', { class: 'field-row' }, field('Changed files', files), field('Code', code)),
            h('div', { class: 'field-row' }, field('Evidence', evidence), field('Workstream', ws)),
            err,
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview save', onclick: () => {
                    const recipients = Array.from(picker.querySelectorAll('input:checked')).map(i => i.value);
                    if (!summary.value.trim()) { fill(err, h('div', { class: 'notice notice-error', text: 'A summary is required.' })); return; }
                    if (audMembers.checked && !recipients.length) { fill(err, h('div', { class: 'notice notice-error', text: 'Pick at least one recipient, or address the team.' })); return; }
                    const out = {
                        summary: summary.value.trim(), audience: audTeam.checked ? 'team' : 'members', recipients: audTeam.checked ? [] : recipients,
                        completed: lines(completed.value), inProgress: lines(inProgress.value), decisions: lines(decisions.value), blockers: lines(blockers.value),
                        unresolvedQuestions: lines(questions.value), nextActions: lines(next.value), changedFiles: lines(files.value),
                        code: lines(code.value).map(parseCodeRef), evidence: lines(evidence.value).map(parseEvidence),
                    };
                    if (title.value.trim()) out.title = title.value.trim();
                    if (ws.value) out.workstream = ws.value;
                    close(out);
                } })),
        ];
    }, { wide: true, sticky: true });
    if (!input) return null;
    const res = await runOperation(Object.assign({ kind: 'relay.draft.save', draft: input }, draft ? { draftId: draft.id } : {}), { title: 'Save relay draft', okText: 'Save draft' });
    if (res) { if (window.location.pathname.startsWith('/relays')) render(); else navigate('/relays'); }
    return res;
}
