// Knobyte Hub entry: session bootstrap, shell (sidebar groups with live
// counts, capability-gated navigation, top bar), routes, keyboard shortcuts.

import {
    $, h, fill, api, post, startSession, route, render, initRouter, navigate, onRender,
    app, onShell, refreshShell, toast, openDialog, badge, link, loading,
} from './core.js';
import { runOperation } from './team.js';
import { startJobLifecycle } from './lifecycle.js';
import * as overview from './pages/overview.js';
import * as understand from './pages/understand.js';
import * as knowledge from './pages/knowledge.js';
import * as search from './pages/search.js';
import * as symbol from './pages/symbol.js';
import * as inbox from './pages/inbox.js';
import * as relays from './pages/relays.js';
import * as members from './pages/members.js';
import * as workstreams from './pages/workstreams.js';
import * as playbooks from './pages/playbooks.js';
import * as catchup from './pages/catchup.js';
import * as specs from './pages/specs.js';
import * as activity from './pages/activity.js';
import * as groundings from './pages/groundings.js';
import * as mcp from './pages/mcp.js';
import * as health from './pages/health.js';
import * as jobs from './pages/jobs.js';
import * as setup from './pages/setup.js';
import * as settings from './pages/settings.js';

const ICONS = {
    home: 'M3 11l9-8 9 8v9a1 1 0 0 1-1 1h-5v-6H9v6H4a1 1 0 0 1-1-1z',
    graph: 'M5 6m-2.5 0a2.5 2.5 0 1 0 5 0a2.5 2.5 0 1 0-5 0M19 6m-2.5 0a2.5 2.5 0 1 0 5 0a2.5 2.5 0 1 0-5 0M12 18m-2.5 0a2.5 2.5 0 1 0 5 0a2.5 2.5 0 1 0-5 0M7 7.5l3.5 8.5M17 7.5l-3.5 8.5M7.5 6h9',
    search: 'M11 11m-7 0a7 7 0 1 0 14 0a7 7 0 1 0-14 0M21 21l-4.35-4.35',
    inbox: 'M22 12h-6l-2 3h-4l-2-3H2M5.45 5.11L2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z',
    spec: 'M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8zM14 2v6h6M8 13h8M8 17h6',
    link: 'M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71',
    relay: 'M22 2L11 13M22 2l-7 20-4-9-9-4z',
    stream: 'M4 6h16M4 12h10M4 18h6',
    pulse: 'M22 12h-4l-3 9L9 3l-3 9H2',
    team: 'M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2M9 7m-4 0a4 4 0 1 0 8 0a4 4 0 1 0-8 0M23 21v-2a4 4 0 0 0-3-3.87M16 3.13a4 4 0 0 1 0 7.75',
    heart: 'M20.8 4.6a5.5 5.5 0 0 0-7.8 0L12 5.7l-1-1.1a5.5 5.5 0 0 0-7.8 7.8l1 1.1L12 21l7.8-7.5 1-1.1a5.5 5.5 0 0 0 0-7.8z',
    jobs: 'M21 12a9 9 0 1 1-6.22-8.56M21 3v6h-6',
    setup: 'M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94z',
    settings: 'M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6',
    fleet: 'M2 2h20v8H2zM2 14h20v8H2zM6 6h.01M6 18h.01',
    chip: 'M4 4h16v16H4zM9 9h6v6H9zM9 1v3M15 1v3M9 20v3M15 20v3',
    checklist: 'M9 6h11M9 12h11M9 18h11M4 6l1 1 2-2M4 12l1 1 2-2M4 18l1 1 2-2',
    history: 'M3 12a9 9 0 1 0 3-6.7L3 8M3 3v5h5M12 7v5l3 3',
};

const NAV = [
    { items: [{ href: '/', label: 'Overview', icon: 'home', exact: true }] },
    {
        title: 'Project', items: [
            { href: '/context', label: 'Context', icon: 'graph', cap: 'scaffold', also: ['/knowledge'] },
            { href: '/search', label: 'Search', icon: 'search', also: ['/code'] },
            { href: '/inbox', label: 'Inbox', icon: 'inbox', cap: 'scaffold', count: 'inboxPending' },
            { href: '/specs', label: 'Specs', icon: 'spec', cap: 'scaffold', count: 'specs', quiet: true },
            { href: '/groundings', label: 'Groundings', icon: 'link', cap: 'scaffold' },
        ],
    },
    {
        title: 'Teamwork', items: [
            { href: '/catch-up', label: 'Catch up', icon: 'history', cap: 'scaffold' },
            { href: '/relays', label: 'Relays', icon: 'relay', cap: 'scaffold', count: 'relaysForMe' },
            { href: '/workstreams', label: 'Workstreams', icon: 'stream', cap: 'scaffold', count: 'workstreams', quiet: true },
            { href: '/playbooks', label: 'Playbooks', icon: 'checklist', cap: 'scaffold' },
            { href: '/activity', label: 'Activity', icon: 'pulse', cap: 'scaffold' },
            { href: '/members', label: 'Team', icon: 'team', cap: 'scaffold', sub: 'actor' },
        ],
    },
    {
        title: 'System', items: [
            { href: '/health', label: 'Health', icon: 'heart' },
            { href: '/jobs', label: 'Jobs', icon: 'jobs', count: 'activeJob' },
            { href: '/setup', label: 'Setup', icon: 'setup', count: 'setup' },
            { href: '/settings', label: 'Settings', icon: 'settings' },
            { href: '/fleet', label: 'Fleet', icon: 'fleet' },
            { href: '/mcp', label: 'MCP', icon: 'chip' },
        ],
    },
];

function icon(name) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('class', 'nav-icon');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('aria-hidden', 'true');
    const p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    p.setAttribute('d', ICONS[name] || ICONS.home);
    svg.appendChild(p);
    return svg;
}

function isActive(item, path) {
    if (item.exact) return path === '/';
    return [item.href].concat(item.also || []).some(p => path === p || path.startsWith(p + '/'));
}

function countFor(item, shell) {
    if (!shell || !item.count) return 0;
    if (item.count === 'activeJob') return shell.activeJob ? '•' : 0;
    if (item.count === 'setup') return shell.capabilities && !shell.capabilities.scaffold ? '!' : 0;
    return (shell.counts || {})[item.count] || 0;
}

function renderNav() {
    const shell = app.shell;
    const caps = (shell && shell.capabilities) || {};
    const path = window.location.pathname;
    const nav = $('nav');
    fill(nav, NAV.map((group, gi) => {
        const items = group.items.map(item => {
            const disabled = item.cap && shell && !caps[item.cap];
            const n = countFor(item, shell);
            const sub = item.sub === 'actor' && shell ? (shell.actor && shell.actor.member ? shell.actor.member.displayName : 'No member selected') : null;
            const inner = [icon(item.icon), h('span', { class: 'nav-label' }, item.label, sub ? h('small', { class: 'nav-sub', text: sub }) : null),
                n ? h('span', { class: 'nav-count' + (item.quiet ? ' quiet' : ''), text: String(n), 'aria-label': n + ' ' + item.label.toLowerCase() }) : null];
            if (disabled) {
                return h('li', null, h('span', { class: 'nav-item disabled', 'aria-disabled': 'true', title: 'Set up Knobyte for this project first' }, inner));
            }
            const active = isActive(item, path);
            return h('li', null, h('a', { href: item.href, 'data-link': '', class: 'nav-item' + (active ? ' active' : ''), 'aria-current': active ? 'page' : null }, inner));
        });
        return h('div', { class: 'nav-group' },
            group.title ? h('div', { class: 'nav-group-title', id: 'navg-' + gi, text: group.title }) : null,
            h('ul', { class: 'nav-list', 'aria-labelledby': group.title ? 'navg-' + gi : null }, items));
    }));
}

function renderTopbar() {
    const shell = app.shell;
    if (!shell) return;
    fill($('topbar-repo'),
        h('span', { class: 'repo-name', text: shell.repo }),
        h('span', { class: 'muted small mono repo-root', text: shell.projectRoot }));
    const g = shell.git || {};
    const meta = [];
    if (g.available) {
        meta.push(h('span', { class: 'meta-item', title: 'Current branch' }, h('span', { class: 'meta-label', text: 'branch' }), h('span', { class: 'mono', text: g.branch || 'detached' })));
        meta.push(h('span', { class: 'meta-item', title: g.head || '' }, h('span', { class: 'meta-label', text: 'HEAD' }), h('span', { class: 'mono', text: g.headShort || '—' })));
        meta.push(g.dirty ? badge(g.changedFiles + ' local change' + (g.changedFiles === 1 ? '' : 's'), 'warning') : badge('clean', 'success'));
    } else {
        meta.push(badge('no git', 'neutral'));
    }
    if (shell.activeJob) {
        meta.push(link('/jobs/' + shell.activeJob.id, shell.activeJob.label + ' · ' + shell.activeJob.phase, 'pill pill-accent'));
    }
    if (shell.capabilities && shell.capabilities.scaffold) {
        const who = shell.actor && shell.actor.member ? shell.actor.member.displayName : 'Choose member';
        meta.push(h('button', { type: 'button', class: 'pill pill-btn', 'aria-haspopup': 'dialog', onclick: switchActor },
            h('span', { class: 'meta-label', text: 'working as' }), h('span', { text: who })));
    }
    fill($('topbar-meta'), meta);
}

/// "Working as" switcher: selects the checkout's current member via preview → apply.
async function switchActor() {
    const data = await api('/api/actor');
    const choice = await openDialog('Working as', close => [
        h('p', { class: 'dialog-text', text: 'Reviews, handoffs and drafts are attributed to the selected member of this checkout. The selection stays local to this checkout.' }),
        data.members.length ? h('div', { class: 'choice-list', role: 'list' }, data.members.map(m => h('button', {
            type: 'button', role: 'listitem', class: 'choice' + (data.member && data.member.id === m.id ? ' selected' : ''),
            onclick: () => close({ id: m.id }),
        }, h('strong', { text: m.displayName }), h('span', { class: 'muted mono', text: '@' + m.id + (m.role ? ' · ' + m.role : '') }))))
            : h('div', { class: 'notice notice-warning', text: 'No active members yet. Add one on the Team page.' }),
        h('div', { class: 'dialog-actions' },
            data.member ? h('button', { type: 'button', class: 'btn', text: 'Clear selection', onclick: () => close({ clear: true }) }) : null,
            link('/members', 'Manage team', 'btn'),
            h('button', { type: 'button', class: 'btn btn-primary', text: 'Close', onclick: () => close(null) })),
    ]);
    if (!choice) return;
    const action = choice.clear ? { kind: 'member.clear' } : { kind: 'member.select', memberId: choice.id };
    const res = await runOperation(action, { title: choice.clear ? 'Clear the working member' : 'Work as @' + choice.id, quick: true });
    if (res) { await refreshShell(); navigate(window.location.pathname + window.location.search, { replace: true }); }
}

function initShortcuts() {
    document.addEventListener('keydown', ev => {
        const t = ev.target;
        const typing = t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName));
        if (ev.key === '/' && !typing && !ev.metaKey && !ev.ctrlKey && !ev.altKey && !document.body.classList.contains('modal-open')) {
            ev.preventDefault();
            openSearch();
        }
        if (ev.key === 'Escape' && document.getElementById('app').classList.contains('nav-open')) closeNav();
    });
    $('search-launch').addEventListener('click', openSearch);
}

function openSearch() {
    if (window.location.pathname !== '/search') navigate('/search');
    setTimeout(() => { const i = document.getElementById('search-input'); if (i) i.focus(); }, 30);
}

function openNav() {
    $('app').classList.add('nav-open');
    $('scrim').hidden = false;
    $('menu-btn').setAttribute('aria-expanded', 'true');
    const first = $('sidebar').querySelector('a, button');
    if (first) first.focus();
}
function closeNav() {
    $('app').classList.remove('nav-open');
    $('scrim').hidden = true;
    $('menu-btn').setAttribute('aria-expanded', 'false');
}

function registerRoutes() {
    route('/', overview.home, { title: 'Overview' });
    route('/fleet', overview.fleet, { title: 'Fleet' });
    route('/context', understand.page, { title: 'Context' });
    route('/knowledge/:id', knowledge.page, { title: 'Knowledge' });
    route('/search', search.page, { title: 'Search' });
    route('/code', search.codeLanding, { title: 'Code' });
    route('/code/symbols/:id', symbol.page, { title: 'Symbol' });
    route('/inbox', inbox.page, { title: 'Inbox' });
    route('/inbox/:id', inbox.page, { title: 'Inbox' });
    route('/specs', specs.page, { title: 'Specs' });
    route('/specs/:id', specs.page, { title: 'Spec' });
    route('/relays', relays.page, { title: 'Relays' });
    route('/relays/:id', relays.page, { title: 'Relay' });
    route('/workstreams', workstreams.page, { title: 'Workstreams' });
    route('/workstreams/:id', workstreams.page, { title: 'Workstream' });
    route('/playbooks', playbooks.page, { title: 'Playbooks' });
    route('/playbooks/runs/:run', playbooks.page, { title: 'Playbook run' });
    route('/playbooks/:id', playbooks.page, { title: 'Playbook' });
    route('/catch-up', catchup.page, { title: 'Catch up' });
    route('/members', members.page, { title: 'Team' });
    route('/members/:id', members.page, { title: 'Member' });
    route('/activity', activity.page, { title: 'Activity' });
    route('/groundings', groundings.page, { title: 'Groundings' });
    route('/mcp', mcp.page, { title: 'MCP' });
    route('/health', health.page, { title: 'Health' });
    route('/jobs', jobs.page, { title: 'Jobs' });
    route('/jobs/:id', jobs.page, { title: 'Job' });
    route('/setup', setup.page, { title: 'Setup' });
    route('/settings', settings.page, { title: 'Settings' });
}

function signInRequired(err) {
    $('app').dataset.state = 'locked';
    fill($('main'), h('div', { class: 'locked' },
        h('h1', { class: 'page-title', text: 'Sign-in link required' }),
        h('p', { class: 'page-desc' }, 'This Hub only answers browsers that opened the one-time link printed by ', h('code', { text: 'knobyte hub' }),
            ' (it ends in ', h('code', { text: '#token=…' }), '). Links expire after five minutes and work once; restart ', h('code', { text: 'knobyte hub' }), ' for a fresh one.'),
        err && err.status !== 401 ? h('p', { class: 'muted', text: err.message }) : null));
}

async function boot() {
    initRouter();
    registerRoutes();
    initShortcuts();
    $('menu-btn').addEventListener('click', openNav);
    $('scrim').addEventListener('click', closeNav);
    $('sidebar-close').addEventListener('click', closeNav);
    onRender(() => { renderNav(); closeNav(); });
    onShell(() => { renderNav(); renderTopbar(); });
    document.addEventListener('hub:unauthorized', () => signInRequired());
    renderNav();
    fill($('main'), loading('Connecting to the local Hub…'));
    try {
        await startSession();
    } catch (err) {
        signInRequired(err);
        return;
    }
    $('app').dataset.state = 'ready';
    await refreshShell();
    if (app.shell && !app.shell.capabilities.scaffold && window.location.pathname === '/') {
        navigate('/setup', { replace: true });
    } else {
        render();
    }
    startJobLifecycle(app.shell && app.shell.projectRoot);
    document.addEventListener('hub:data-refresh', refreshData);
    setInterval(() => { if (!document.hidden) refreshShell(); }, 15000);
    document.addEventListener('visibilitychange', () => { if (!document.hidden) refreshShell(); });
    maybeTour();
}

/// A job finished (in this tab or another): refresh the shell and re-render the
/// current page, unless the user is in a dialog or typing (then only the shell).
async function refreshData() {
    await refreshShell();
    const t = document.activeElement;
    const typing = t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName));
    if (document.body.classList.contains('modal-open') || typing) return;
    const y = window.scrollY;
    await render();
    window.scrollTo(0, y);
}

async function maybeTour() {
    if (!app.shell || !app.shell.capabilities.scaffold) return;
    try {
        const o = await api('/api/settings/onboarding');
        if (!o.completed) settings.showTour();
    } catch (_) { /* optional */ }
}

window.addEventListener('hub:shell-refresh', () => refreshShell());
export { post, toast };

boot();
