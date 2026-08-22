// Team mutations: every write is preview → (user confirms) → apply of the
// exact signed envelope. A stale envelope is refused by the server; the user
// can preview again.

import { h, post, openDialog, toast, badge, refreshShell } from './core.js';

function changeRow(c) {
    const kindCls = c.kind === 'create' ? 'success' : c.kind === 'delete' ? 'danger' : 'warning';
    return h('li', { class: 'change-row' },
        badge(c.kind, kindCls),
        h('span', { class: 'mono change-path', text: c.path }),
        h('span', { class: 'muted', text: c.summary }),
        c.namespace === 'local' ? badge('local only', 'neutral') : badge('shared via git', 'accent'));
}

/// Parse one evidence line (`entity:<id>`, `code:<symbol>`, `commit:<hash>`,
/// `file:<path>`, `external:<uri>` or a URL, `manual:<note>`; other text is a note).
export function parseEvidence(line) {
    const spec = String(line || '').trim();
    if (/^https?:\/\//.test(spec)) return { kind: 'external', uri: spec };
    const i = spec.indexOf(':');
    const k = i > 0 ? spec.slice(0, i) : '';
    const v = i > 0 ? spec.slice(i + 1).trim() : '';
    if (v) {
        if (k === 'entity') return { kind: 'entity', id: v };
        if (k === 'code') return { kind: 'code', symbolId: v };
        if (k === 'commit') return { kind: 'commit', hash: v };
        if (k === 'file') return { kind: 'file', path: v };
        if (k === 'external') return { kind: 'external', uri: v };
        if (k === 'manual') return { kind: 'manual', note: v };
    }
    return { kind: 'manual', note: spec };
}

export function evidenceText(e) {
    switch (e.kind) {
        case 'entity': return 'entity:' + e.id;
        case 'code': return 'code:' + e.symbolId;
        case 'commit': return 'commit:' + e.hash;
        case 'file': return 'file:' + e.path;
        case 'external': return e.label ? e.label + ' <' + e.uri + '>' : e.uri;
        default: return e.note || '';
    }
}

/// Parse a code reference (`symbol:<id>` / `file:<path>`; paths with `/` or an extension are files).
export function parseCodeRef(line) {
    const spec = String(line || '').trim();
    if (spec.startsWith('symbol:')) return { kind: 'symbol', symbolId: spec.slice(7).trim() };
    if (spec.startsWith('file:')) return { kind: 'file', path: spec.slice(5).trim() };
    if (spec.includes('/') || /\.[A-Za-z0-9]+$/.test(spec)) return { kind: 'file', path: spec };
    return { kind: 'symbol', symbolId: spec };
}

export function codeRefText(c) { return c.kind === 'file' ? 'file:' + c.path : 'symbol:' + c.symbolId; }

export function actorLabel(a) {
    if (!a) return 'unknown';
    if (a.kind === 'member') return '@' + a.memberId + (a.displayName ? ' (' + a.displayName + ')' : '');
    if (a.kind === 'git') return 'git: ' + (a.name || a.email || 'unknown');
    return 'unknown actor';
}

function previewBody(env) {
    const p = env.preview;
    return [
        h('p', { class: 'dialog-text' }, h('strong', { text: p.summary })),
        h('div', { class: 'muted small' }, 'Acting as ', h('strong', { text: actorLabel(env.receipt.authority.actor) }),
            ' · scope ', h('strong', { text: p.scope })),
        p.changes.length ? h('ul', { class: 'change-list', 'aria-label': 'Planned file changes' }, p.changes.map(changeRow))
            : h('div', { class: 'notice', text: 'No file changes: the state already matches.' }),
        (p.diagnostics || []).length ? h('ul', { class: 'diag-list' }, p.diagnostics.map(d => h('li', { class: 'notice notice-' + (d.severity === 'error' ? 'error' : 'warning'), text: d.message }))) : null,
    ];
}

/// Preview `action`, ask for confirmation, then apply. Resolves with the apply
/// result, or null when cancelled or refused.
/// opts: { title, okText, danger, quick (skip dialog when no diagnostics), operationId,
///         hint(err) -> string|null (a Hub-specific remedy shown instead of a CLI-oriented one) }
export async function runOperation(action, opts) {
    opts = opts || {};
    for (;;) {
        let env;
        try {
            env = (await post('/api/team/operations/preview', { action, operationId: opts.operationId })).envelope;
        } catch (err) {
            const hint = opts.hint ? opts.hint(err) : null;
            await openDialog(opts.title || 'Cannot prepare this change', close => [
                h('div', { class: 'notice notice-error', role: 'alert' }, h('strong', { text: err.title || 'Refused' }), ' ', hint || err.message),
                h('div', { class: 'dialog-actions' }, h('button', { type: 'button', class: 'btn btn-primary', text: 'Close', autofocus: true, onclick: () => close() })),
            ]);
            return null;
        }
        const needsConfirm = !(opts.quick && !(env.preview.diagnostics || []).length);
        if (needsConfirm) {
            const ok = await openDialog(opts.title || 'Review change', close => [
                previewBody(env),
                h('div', { class: 'dialog-actions' },
                    h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(false) }),
                    h('button', { type: 'button', class: 'btn ' + (opts.danger ? 'btn-danger' : 'btn-primary'), text: opts.okText || 'Apply', autofocus: true, onclick: () => close(true) })),
            ], { wide: true });
            if (!ok) return null;
        }
        try {
            const res = await post('/api/team/operations/apply', { envelope: env });
            toast(res.summary || 'Applied', 'success');
            refreshShell();
            return res;
        } catch (err) {
            const stale = err.status === 409;
            const again = await openDialog(stale ? 'Something changed' : 'Change refused', close => [
                h('div', { class: 'notice ' + (stale ? 'notice-warning' : 'notice-error'), role: 'alert' }, (opts.hint && opts.hint(err)) || err.message),
                stale ? h('p', { class: 'dialog-text', text: 'Nothing was written. Preview again to see the current plan.' }) : null,
                h('div', { class: 'dialog-actions' },
                    h('button', { type: 'button', class: 'btn', text: 'Close', onclick: () => close(false) }),
                    stale ? h('button', { type: 'button', class: 'btn btn-primary', text: 'Preview again', autofocus: true, onclick: () => close(true) }) : null),
            ]);
            if (!again) return null;
        }
    }
}
