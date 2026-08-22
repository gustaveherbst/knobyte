// Team: members with contribution metrics; add / update / deactivate /
// reactivate / select / clear (preview → apply); member detail with history.

import { h, fill, api, pageHeader, card, empty, badge, statusBadge, fmtTime, link, enc, kv, openDialog, field, lines, app, render, initials, refreshShell } from '../core.js';
import { runOperation } from '../team.js';

/// Git aliases as editable lines: `Name <email>`, `email`, or `Name`.
export function aliasText(a) {
    if (a.name && a.email) return a.name + ' <' + a.email + '>';
    return a.email || a.name || '';
}

export function parseAliases(text) {
    return lines(text).map(l => {
        const m = l.match(/^(.*?)\s*<([^<>\s]+)>$/);
        if (m) return { name: m[1].trim() || undefined, email: m[2].trim() };
        return l.includes('@') && !/\s/.test(l) ? { email: l } : { name: l };
    });
}

function current() { return app.shell && app.shell.actor && app.shell.actor.member ? app.shell.actor.member.id : null; }

async function memberForm(title, m, isNew) {
    m = m || {};
    return openDialog(title, close => {
        const id = h('input', { class: 'input mono', id: 'mem-id', maxlength: 64, value: m.id || '', disabled: !isNew, autofocus: isNew, placeholder: 'ada' });
        const name = h('input', { class: 'input', id: 'mem-name', maxlength: 128, value: m.displayName || '', autofocus: !isNew });
        const email = h('input', { class: 'input', id: 'mem-email', type: 'email', maxlength: 256, value: m.email || '' });
        const role = h('input', { class: 'input', id: 'mem-role', maxlength: 128, value: m.role || '' });
        const aliases = h('textarea', { class: 'input', id: 'mem-aliases', value: (m.gitAliases || []).map(aliasText).join('\n'), placeholder: 'Ada Lovelace <ada@example.com>' });
        const sel = h('input', { type: 'checkbox', id: 'mem-select', checked: isNew && !current() });
        const err = h('div', { role: 'alert' });
        return [
            field('Member id', id, isNew ? "Letters, digits, '.', '-' and '_'. Used in attribution; cannot change later." : null),
            field('Display name', name), field('Email (optional)', email), field('Role (optional)', role),
            field('Git aliases (optional)', aliases, 'Other Git names or emails this member commits as, one per line: Name <email>, an email, or a name.'),
            isNew ? h('label', { class: 'check', for: 'mem-select' }, sel, ' Work as this member in this checkout') : null,
            err,
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview', onclick: () => {
                    if (isNew && !/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(id.value.trim())) { fill(err, h('div', { class: 'notice notice-error', text: "Use letters, digits, '.', '-' or '_' (max 128)." })); return; }
                    if (!name.value.trim()) { fill(err, h('div', { class: 'notice notice-error', text: 'A display name is required.' })); return; }
                    close({ id: id.value.trim(), displayName: name.value.trim(), email: email.value.trim(), role: role.value.trim(), gitAliases: parseAliases(aliases.value), select: sel.checked });
                } })),
        ];
    });
}

export async function addMember() {
    const v = await memberForm('Add a team member', null, true);
    if (!v) return;
    const member = { id: v.id, displayName: v.displayName };
    if (v.email) member.email = v.email;
    if (v.role) member.role = v.role;
    if (v.gitAliases.length) member.gitAliases = v.gitAliases;
    const res = await runOperation({ kind: 'member.add', member }, { title: 'Add @' + v.id, okText: 'Add member' });
    if (res && v.select) await runOperation({ kind: 'member.select', memberId: v.id }, { title: 'Work as @' + v.id, quick: true });
    if (res) { await refreshShell(); render(); }
}

async function editMember(m) {
    const v = await memberForm('Update @' + m.id, m, false);
    if (!v) return;
    const patch = {};
    if (v.displayName !== m.displayName) patch.displayName = v.displayName;
    if ((v.email || '') !== (m.email || '')) patch.email = v.email;
    if ((v.role || '') !== (m.role || '')) patch.role = v.role;
    if (JSON.stringify(v.gitAliases.map(aliasText)) !== JSON.stringify((m.gitAliases || []).map(aliasText))) patch.gitAliases = v.gitAliases;
    if (await runOperation({ kind: 'member.update', memberId: m.id, patch }, { title: 'Update @' + m.id })) { await refreshShell(); render(); }
}

async function act(kind, m, opts) {
    if (await runOperation(Object.assign({ kind }, kind === 'member.clear' ? {} : { memberId: m.id }), opts)) { await refreshShell(); render(); }
}

function actions(m) {
    const cur = current();
    return h('div', { class: 'row-actions' },
        m.status === 'active' && cur !== m.id ? h('button', { type: 'button', class: 'btn btn-sm btn-primary', text: 'Work as', onclick: () => act('member.select', m, { title: 'Work as @' + m.id, quick: true }) }) : null,
        cur === m.id ? h('button', { type: 'button', class: 'btn btn-sm', text: 'Clear selection', onclick: () => act('member.clear', m, { title: 'Stop working as @' + m.id }) }) : null,
        h('button', { type: 'button', class: 'btn btn-sm', text: 'Edit', onclick: () => editMember(m) }),
        m.status === 'active'
            ? h('button', { type: 'button', class: 'btn btn-sm btn-danger', text: 'Deactivate', onclick: () => act('member.deactivate', m, { title: 'Deactivate @' + m.id, okText: 'Deactivate', danger: true }) })
            : h('button', { type: 'button', class: 'btn btn-sm', text: 'Reactivate', onclick: () => act('member.reactivate', m, { title: 'Reactivate @' + m.id, okText: 'Reactivate' }) }));
}

export async function page(ctx) {
    const { main, params } = ctx;
    if (params.id) return detail(ctx);
    const show = ctx.query.get('show') || 'active';
    const [membersPage, contrib] = await Promise.all([
        api('/api/team/members?limit=100' + (show === 'all' ? '' : '&active=' + (show === 'active'))),
        api('/api/contributors').catch(() => ({ contributors: [] })),
    ]);
    const stats = new Map((contrib.contributors || []).map(c => [c.id, c]));
    const cur = current();
    fill(main,
        pageHeader('Team', 'Members of this project. Selecting a member decides who reviews, hands off and drafts in this checkout.',
            h('button', { type: 'button', class: 'btn btn-primary', text: 'Add member', onclick: addMember })),
        !cur ? h('div', { class: 'notice notice-warning', text: 'No member is selected for this checkout. Use "Work as" on your own card.' }) : null,
        h('div', { class: 'chip-row', role: 'group', 'aria-label': 'Show' }, [['active', 'Active'], ['inactive', 'Inactive'], ['all', 'All']].map(([v, l]) =>
            h('a', { href: '/members' + (v === 'active' ? '' : '?show=' + v), 'data-link': '', class: 'chip' + (show === v ? ' active' : ''), 'aria-pressed': String(show === v), text: l }))),
        membersPage.items.length ? h('div', { class: 'member-grid' }, membersPage.items.map(m => {
            const c = stats.get(m.id);
            return h('article', { class: 'member-card' + (cur === m.id ? ' current' : '') },
                h('div', { class: 'member-head' },
                    h('div', { class: 'avatar', 'aria-hidden': 'true', text: initials(m.displayName) }),
                    h('div', { class: 'grow' }, link('/members/' + enc(m.id), m.displayName, 'member-name'), h('div', { class: 'muted small mono', text: '@' + m.id + (m.role ? ' · ' + m.role : '') })),
                    cur === m.id ? badge('you', 'accent') : statusBadge(m.status)),
                c ? h('div', { class: 'member-metrics' },
                    h('span', null, h('strong', { text: c.decisions_count }), ' decisions'), h('span', null, h('strong', { text: c.discoveries_count }), ' discoveries'),
                    h('span', null, h('strong', { text: c.relays_authored }), ' relays'), c.in_flight_relay ? h('span', { class: 'muted', text: 'in flight: ' + c.in_flight_relay }) : null) : null,
                actions(m));
        })) : empty(show === 'active' ? 'No active members yet. Add yourself to start attributing reviews and handoffs.' : 'Nobody here.'));
}

async function detail(ctx) {
    const d = await api('/api/team/members/' + enc(ctx.params.id));
    const m = d.member;
    fill(ctx.main,
        link('/members', '← Team', 'back-link'),
        pageHeader(m.displayName, h('span', { class: 'mono', text: '@' + m.id })),
        h('div', { class: 'two-col' },
            card('Profile', [kv([['Status', statusBadge(m.status)], ['Role', m.role], ['Email', m.email], ['Git aliases', (m.gitAliases || []).map(a => a.email || a.name).join(', ')],
                ['Created', fmtTime(m.createdAt)], ['Updated', fmtTime(m.updatedAt)]]), actions(m)]),
            card('Recent team activity', d.activity.length ? h('ul', { class: 'memory-list' }, d.activity.map(a => h('li', { class: 'memory-item' },
                h('div', { class: 'memory-head' }, badge(a.action, 'accent'), h('span', { class: 'muted small', text: fmtTime(a.timestamp) })),
                h('div', { class: 'memory-summary', text: a.summary })))) : empty('No team activity recorded for this member.'))));
}
