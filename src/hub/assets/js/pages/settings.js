// Settings: agent logging cadence, onboarding tour (and its reset), session.

import { h, fill, api, post, pageHeader, card, toast, openDialog, errorBox, badge, fmtTime } from '../core.js';

const MODE_HELP = {
    significant: 'Agents log decisions and discoveries as they happen (default).',
    checkpoints: 'Agents log at natural checkpoints (end of a task or handoff).',
    manual: 'Agents only log when you ask them to.',
};

const TOUR = [
    ['Welcome to the Project Hub', 'The Hub is a local workstation for your project memory: knowledge, code context and team handoffs. It runs on this machine; shared records travel through git.'],
    ['Overview tells you what is next', 'The Overview ranks what needs you: handoffs waiting, proposals to review, stale context. Start there each session.'],
    ['Context and Search', 'Context shows wiki entities grounded in code. Search fuses full-text and semantic matches; press / anywhere to search. Symbol pages show callers, callees and impact.'],
    ['Every change is previewed', 'Reviews, relays and team changes are previewed first: you see exactly which files change before anything is written. If something changed meanwhile, the Hub asks you to preview again.'],
    ['Keep context healthy', 'Health shows whether the code graph and wiki match your working tree. Refresh or rebuild them as background jobs from Health or Jobs.'],
];

/// Show the onboarding tour; marks it completed when finished or skipped.
export async function showTour() {
    let i = 0;
    const done = await openDialog('Getting started', close => {
        const title = h('h3', { class: 'tour-title' });
        const text = h('p', { class: 'dialog-text' });
        const dots = h('div', { class: 'tour-dots', 'aria-hidden': 'true' });
        const back = h('button', { type: 'button', class: 'btn', text: 'Back' });
        const next = h('button', { type: 'button', class: 'btn btn-primary', autofocus: true });
        const counter = h('span', { class: 'muted small', role: 'status' });
        const draw = () => {
            title.textContent = TOUR[i][0];
            text.textContent = TOUR[i][1];
            counter.textContent = 'Step ' + (i + 1) + ' of ' + TOUR.length;
            fill(dots, TOUR.map((_, k) => h('span', { class: 'tour-dot' + (k === i ? ' on' : '') })));
            back.disabled = i === 0;
            next.textContent = i === TOUR.length - 1 ? 'Finish' : 'Next';
        };
        back.addEventListener('click', () => { if (i > 0) { i--; draw(); } });
        next.addEventListener('click', () => { if (i < TOUR.length - 1) { i++; draw(); next.focus(); } else close(true); });
        draw();
        return [title, text, dots, h('div', { class: 'dialog-actions' }, counter,
            h('button', { type: 'button', class: 'btn btn-ghost', text: 'Skip tour', onclick: () => close(true) }), back, next)];
    });
    if (done !== undefined) {
        try { await post('/api/settings/onboarding', { completed: true }); } catch (_) { /* best effort */ }
    }
}

export async function page(ctx) {
    const { main } = ctx;
    const [logging, onboarding, session] = await Promise.all([
        api('/api/settings/logging').catch(err => ({ error: err })),
        api('/api/settings/onboarding').catch(() => ({ completed: false })),
        api('/api/session').catch(() => null),
    ]);
    const loggingCard = logging.error ? card('Agent logging cadence', errorBox(logging.error)) : card('Agent logging cadence', [
        h('p', { class: 'muted', text: 'How often agents in this checkout record decisions and discoveries. Stored locally in .knobyte/local/.' }),
        h('fieldset', { class: 'radio-list' }, h('legend', { class: 'visually-hidden', text: 'Logging mode' }), logging.modes.map(m => h('label', { class: 'radio-card', for: 'log-' + m },
            h('input', { type: 'radio', name: 'logging', id: 'log-' + m, value: m, checked: logging.mode === m, onchange: async () => {
                try {
                    const r = await post('/api/settings/logging', { mode: m, expectedRevision: logging.revision || 'none' });
                    Object.assign(logging, r);
                    toast('Logging cadence: ' + m, 'success');
                } catch (err) { toast(err.message, 'error'); page(ctx); }
            } }),
            h('span', null, h('strong', { text: m }), h('span', { class: 'muted small', text: ' ' + MODE_HELP[m] }))))),
        h('div', { class: 'muted small', text: 'Source: ' + logging.source }),
    ]);
    fill(main,
        pageHeader('Settings'),
        loggingCard,
        card('Onboarding', [
            h('p', null, 'Tour status: ', onboarding.completed ? badge('completed ' + fmtTime(onboarding.completedAt), 'success') : badge('not completed', 'neutral')),
            h('div', { class: 'action-row' },
                h('button', { type: 'button', class: 'btn', text: 'Show the tour', onclick: () => showTour().then(() => page(ctx)) }),
                h('button', { type: 'button', class: 'btn', text: 'Reset onboarding', onclick: async () => {
                    try { await post('/api/settings/onboarding', { completed: false }); toast('Onboarding reset; the tour shows on the next visit', 'success'); page(ctx); } catch (err) { toast(err.message, 'error'); }
                } })),
        ]),
        card('Session', [
            h('p', { class: 'muted', text: 'This browser holds a Hub session cookie (HttpOnly, SameSite=Strict, scoped to /api). Restarting the Hub ends every session.' }),
            session && session.expiresAt ? h('p', null, 'Expires ', h('strong', { text: fmtTime(session.expiresAt) })) : null,
            h('div', { class: 'action-row' }, h('button', { type: 'button', class: 'btn btn-danger', text: 'Sign out', onclick: async () => {
                try { await post('/api/session/logout', {}); } catch (_) { /* ignore */ }
                document.dispatchEvent(new CustomEvent('hub:unauthorized'));
            } })),
        ]));
}

export { openDialog };
