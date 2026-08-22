// Knobyte Hub core: DOM helpers, API client (session + CSRF + problem+json),
// router, dialogs (focus-trapped), toasts and formatting.
// Untrusted strings are only inserted with textContent / DOM APIs.

export const SVG_NS = 'http://www.w3.org/2000/svg';

// ---------------------------------------------------------------------------
// DOM
// ---------------------------------------------------------------------------

export function h(tag, attrs, ...children) {
    const el = document.createElement(tag);
    applyAttrs(el, attrs);
    appendChildren(el, children);
    return el;
}

export function s(tag, attrs, ...children) {
    const el = document.createElementNS(SVG_NS, tag);
    applyAttrs(el, attrs);
    appendChildren(el, children);
    return el;
}

function applyAttrs(el, attrs) {
    if (!attrs) return;
    for (const [k, v] of Object.entries(attrs)) {
        if (v === undefined || v === null || v === false) continue;
        if (k === 'class') el.setAttribute('class', v);
        else if (k === 'text') el.textContent = String(v);
        else if (k === 'dataset') Object.assign(el.dataset, v);
        else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
        else if (k === 'style') throw new Error('inline style attributes are not allowed (CSP)');
        else if (k === 'value' && 'value' in el) el.value = v;
        else if (k === 'checked' && 'checked' in el) el.checked = !!v;
        else el.setAttribute(k, v === true ? '' : String(v));
    }
}

function appendChildren(el, children) {
    for (const c of children.flat(Infinity)) {
        if (c === undefined || c === null || c === false) continue;
        el.appendChild(c instanceof Node ? c : document.createTextNode(String(c)));
    }
}

export const $ = id => document.getElementById(id);
export function fill(el, ...children) { el.replaceChildren(); appendChildren(el, children); return el; }
export function empty(text, ...extra) { return h('div', { class: 'empty-state' }, text, ...extra); }
export function errorBox(err) {
    return h('div', { class: 'notice notice-error', role: 'alert' },
        h('strong', { text: (err && err.title) || 'Something went wrong' }), ' ',
        h('span', { text: (err && err.message) || String(err) }));
}
export function loading(text) { return h('div', { class: 'loading', role: 'status' }, h('span', { class: 'spinner', 'aria-hidden': 'true' }), text || 'Loading…'); }

export function badge(text, kind) { return h('span', { class: 'badge badge-' + (kind || 'neutral'), text }); }

const STATUS_KIND = {
    pending: 'warning', approved: 'success', rejected: 'danger', stale: 'warning', withdrawn: 'neutral',
    published: 'accent', acknowledged: 'warning', closed: 'neutral',
    healthy: 'success', warning: 'warning', drifted: 'danger', unavailable: 'danger', degraded: 'warning',
    fresh: 'success', missing: 'danger', corrupt: 'danger', rebuild_required: 'danger',
    active: 'success', inactive: 'neutral', draft: 'neutral', planned: 'neutral', blocked: 'danger', done: 'success', archived: 'neutral',
    completed: 'success', abandoned: 'neutral',
    queued: 'neutral', running: 'accent', succeeded: 'success', failed: 'danger', interrupted: 'warning', cancelled: 'warning', paused: 'warning', idle: 'neutral',
};
export function statusBadge(status) { return badge(String(status || 'unknown').replace(/_/g, ' '), STATUS_KIND[status] || 'neutral'); }

export function fmtTime(ts) {
    if (!ts) return '—';
    const d = new Date(ts);
    if (isNaN(d.getTime())) return String(ts).substring(0, 19).replace('T', ' ');
    return d.toLocaleString([], { year: 'numeric', month: 'short', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false });
}
export function fmtNum(n) { return typeof n === 'number' ? n.toLocaleString() : '—'; }
export function truncate(str, n) { str = String(str || ''); return str.length > n ? str.substring(0, n - 1) + '…' : str; }
export function initials(name) {
    const i = String(name || '').split(/\s+/).filter(Boolean).map(p => p[0]).join('').substring(0, 2).toUpperCase();
    return i || '?';
}
export const enc = encodeURIComponent;

export function kv(rows) {
    return h('dl', { class: 'kv' }, rows.filter(Boolean).map(([k, v]) => [h('dt', { text: k }), h('dd', null, v instanceof Node ? v : String(v === undefined || v === null || v === '' ? '—' : v))]));
}

export function pageHeader(title, desc, ...actions) {
    return h('header', { class: 'page-head' },
        h('div', null, h('h1', { class: 'page-title', text: title }), desc ? h('p', { class: 'page-desc' }, desc) : null),
        actions.length ? h('div', { class: 'page-actions' }, actions) : null);
}

export function card(title, body, opts) {
    opts = opts || {};
    return h('section', { class: 'card' + (opts.class ? ' ' + opts.class : ''), 'aria-label': typeof title === 'string' ? title : undefined },
        title ? h('div', { class: 'card-head' }, h('h2', { class: 'card-title', text: title }), opts.aside || null) : null,
        h('div', { class: 'card-body' }, body));
}

export function link(href, text, cls) { return h('a', { href, 'data-link': '', class: cls || null, text }); }

let toastTimer = null;
export function toast(msg, kind) {
    const t = $('toast');
    t.textContent = msg;
    t.className = 'toast show' + (kind ? ' ' + kind : '');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { t.className = 'toast'; }, 4200);
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

const session = { csrf: null, ready: false };

export class ApiError extends Error {
    constructor(problem, status) {
        super((problem && (problem.detail || problem.error)) || ('Request failed (' + status + ')'));
        this.status = status;
        this.title = problem && problem.title;
        this.code = problem && problem.code;
        this.problem = problem;
    }
}

async function request(path, opts) {
    const init = Object.assign({ credentials: 'same-origin' }, opts || {});
    init.headers = Object.assign({ Accept: 'application/json' }, init.headers || {});
    const res = await fetch(path, init);
    let data = null;
    const text = await res.text();
    if (text) { try { data = JSON.parse(text); } catch (_) { data = null; } }
    if (!res.ok) {
        const err = new ApiError(data, res.status);
        if (res.status === 401 && session.ready) {
            session.ready = false;
            document.dispatchEvent(new CustomEvent('hub:unauthorized'));
        }
        throw err;
    }
    return data;
}

export function api(path) { return request(path); }

export function post(path, body) {
    return request(path, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-Knobyte-CSRF': session.csrf || '' },
        body: JSON.stringify(body || {}),
    });
}

/// Exchange a `#token=` bootstrap link for a session, then load the CSRF token.
export async function startSession() {
    const m = /(?:^|[#&])token=([^&]+)/.exec(window.location.hash || '');
    if (m) {
        const token = decodeURIComponent(m[1]);
        history.replaceState(null, '', window.location.pathname + window.location.search);
        try {
            await request('/api/session/bootstrap', {
                method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ token }),
            });
        } catch (err) {
            // A used token is fine when this browser already has a session.
            if (err.status !== 401) throw err;
        }
    }
    const sess = await request('/api/session');
    session.csrf = sess.csrfToken;
    session.ready = true;
    return sess;
}

/// Server-sent events for `path`, closed on `done`; returns a closer.
export function stream(path, handlers) {
    const es = new EventSource(path, { withCredentials: true });
    for (const [name, fn] of Object.entries(handlers)) {
        if (name === 'error') continue;
        es.addEventListener(name, ev => { let d = null; try { d = JSON.parse(ev.data); } catch (_) { d = ev.data; } fn(d, ev); });
    }
    // The server ends a stream whose session expired or logged out: stop reconnecting.
    es.addEventListener('session-ended', () => { es.close(); document.dispatchEvent(new CustomEvent('hub:unauthorized')); });
    es.onerror = () => { if (handlers.error) handlers.error(es); };
    return () => es.close();
}

// ---------------------------------------------------------------------------
// Router (history API)
// ---------------------------------------------------------------------------

const routes = [];
let currentCleanup = null;
let navToken = 0;

export function route(pattern, handler, meta) {
    const keys = [];
    const re = new RegExp('^' + pattern.replace(/\/:([a-zA-Z]+)/g, (_, k) => { keys.push(k); return '/([^/]+)'; }) + '/?$');
    routes.push({ pattern, re, keys, handler, meta: meta || {} });
}

export function matchRoute(path) {
    for (const r of routes) {
        const m = r.re.exec(path);
        if (m) {
            const params = {};
            r.keys.forEach((k, i) => { params[k] = decodeURIComponent(m[i + 1]); });
            return { route: r, params };
        }
    }
    return null;
}

export function navigate(href, opts) {
    const url = new URL(href, window.location.origin);
    if (url.origin !== window.location.origin) { window.location.href = href; return; }
    if (opts && opts.replace) history.replaceState(null, '', url.pathname + url.search);
    else history.pushState(null, '', url.pathname + url.search);
    render();
}

export function query() { return new URLSearchParams(window.location.search); }

export function setQuery(params) {
    const q = query();
    for (const [k, v] of Object.entries(params)) {
        if (v === null || v === undefined || v === '') q.delete(k); else q.set(k, v);
    }
    const qs = q.toString();
    history.replaceState(null, '', window.location.pathname + (qs ? '?' + qs : ''));
}

const renderListeners = [];
export function onRender(fn) { renderListeners.push(fn); }

export async function render() {
    const token = ++navToken;
    if (currentCleanup) { try { currentCleanup(); } catch (_) { /* ignore */ } currentCleanup = null; }
    const main = $('main');
    const found = matchRoute(window.location.pathname);
    renderListeners.forEach(fn => fn(found));
    if (!found) {
        fill(main, notFound());
        document.title = 'Not found · Knobyte Hub';
        return;
    }
    document.title = (found.route.meta.title || 'Hub') + ' · Knobyte Hub';
    fill(main, loading());
    try {
        const ctx = { params: found.params, query: query(), main, isCurrent: () => token === navToken, cleanup: fn => { currentCleanup = fn; } };
        await found.route.handler(ctx);
    } catch (err) {
        if (token !== navToken) return;
        if (err && err.status === 404) fill(main, notFound(err.message));
        else fill(main, pageHeader(found.route.meta.title || 'Error'), errorBox(err));
    }
    if (token === navToken && !(window.history.state && window.history.state.keepFocus)) main.focus({ preventScroll: true });
}

function notFound(detail) {
    return h('div', { class: 'not-found' },
        h('div', { class: 'nf-code', text: '404' }),
        h('h1', { class: 'page-title', text: 'Page not found' }),
        h('p', { class: 'page-desc', text: detail || 'Nothing lives at ' + window.location.pathname + '.' }),
        h('div', { class: 'action-row' }, link('/', 'Back to overview', 'btn btn-primary'), link('/search', 'Search the project', 'btn')));
}

export function initRouter() {
    document.addEventListener('click', ev => {
        const a = ev.target.closest('a[data-link]');
        if (!a || ev.defaultPrevented || ev.button !== 0 || ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey) return;
        const href = a.getAttribute('href');
        if (!href || href.startsWith('http')) return;
        ev.preventDefault();
        navigate(href);
    });
    window.addEventListener('popstate', () => render());
}

// ---------------------------------------------------------------------------
// Dialogs (focus trap, Escape, restore focus)
// ---------------------------------------------------------------------------

const FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

export function focusTrap(container, onEscape) {
    const handler = ev => {
        if (ev.key === 'Escape' && onEscape) { ev.preventDefault(); ev.stopPropagation(); onEscape(); return; }
        if (ev.key !== 'Tab') return;
        const items = Array.from(container.querySelectorAll(FOCUSABLE)).filter(el => el.offsetParent !== null || el === document.activeElement);
        if (!items.length) { ev.preventDefault(); return; }
        const first = items[0], last = items[items.length - 1];
        if (ev.shiftKey && document.activeElement === first) { ev.preventDefault(); last.focus(); }
        else if (!ev.shiftKey && document.activeElement === last) { ev.preventDefault(); first.focus(); }
    };
    container.addEventListener('keydown', handler);
    return () => container.removeEventListener('keydown', handler);
}

/// Open a modal dialog. `build(close)` returns the body nodes. Resolves with the close value.
export function openDialog(title, build, opts) {
    opts = opts || {};
    return new Promise(resolve => {
        const previous = document.activeElement;
        const host = $('dialogs');
        const titleId = 'dlg-' + Math.random().toString(36).slice(2);
        let release = null;
        const close = value => {
            if (release) release();
            overlay.remove();
            document.body.classList.toggle('modal-open', host.childElementCount > 0);
            if (previous && previous.focus) previous.focus();
            resolve(value);
        };
        const panel = h('div', { class: 'dialog' + (opts.wide ? ' dialog-wide' : ''), role: opts.alert ? 'alertdialog' : 'dialog', 'aria-modal': 'true', 'aria-labelledby': titleId },
            h('div', { class: 'dialog-head' },
                h('h2', { id: titleId, class: 'dialog-title', text: title }),
                h('button', { type: 'button', class: 'icon-btn', 'aria-label': 'Close', text: '×', onclick: () => close(undefined) })),
            h('div', { class: 'dialog-body' }, build(close)));
        const overlay = h('div', { class: 'dialog-overlay', onmousedown: ev => { if (ev.target === overlay && !opts.sticky) close(undefined); } }, panel);
        host.appendChild(overlay);
        document.body.classList.add('modal-open');
        release = focusTrap(panel, () => close(undefined));
        const target = panel.querySelector('[autofocus]') || panel.querySelector('.dialog-body ' + FOCUSABLE) || panel.querySelector(FOCUSABLE);
        if (target) target.focus();
    });
}

/// Confirmation dialog; resolves true/false.
export function confirmDialog(title, message, opts) {
    opts = opts || {};
    return openDialog(title, close => [
        typeof message === 'string' ? h('p', { class: 'dialog-text', text: message }) : message,
        opts.typeToConfirm ? h('p', { class: 'muted', text: 'Type "' + opts.typeToConfirm + '" to confirm.' }) : null,
        opts.typeToConfirm ? h('input', { class: 'input', id: 'confirm-type', 'aria-label': 'Confirmation text', autocomplete: 'off',
            oninput: ev => { ev.target.closest('.dialog').querySelector('.confirm-ok').disabled = ev.target.value.trim() !== opts.typeToConfirm; } }) : null,
        h('div', { class: 'dialog-actions' },
            h('button', { type: 'button', class: 'btn', text: opts.cancelText || 'Cancel', onclick: () => close(false) }),
            h('button', { type: 'button', class: 'btn confirm-ok ' + (opts.danger ? 'btn-danger' : 'btn-primary'), text: opts.okText || 'Confirm', disabled: !!opts.typeToConfirm, autofocus: !opts.typeToConfirm, onclick: () => close(true) })),
    ], { alert: true }).then(v => v === true);
}

/// Prompt for a single text value.
export function promptDialog(title, label, opts) {
    opts = opts || {};
    return openDialog(title, close => {
        const input = opts.multiline
            ? h('textarea', { class: 'input', 'aria-label': label, maxlength: opts.maxlength || 4000, autofocus: true, value: opts.value || '' })
            : h('input', { class: 'input', 'aria-label': label, maxlength: opts.maxlength || 512, autofocus: true, value: opts.value || '' });
        return [
            h('label', { class: 'field' }, h('span', { class: 'field-label', text: label }), input),
            opts.help ? h('p', { class: 'muted', text: opts.help }) : null,
            h('div', { class: 'dialog-actions' },
                h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
                h('button', { type: 'button', class: 'btn btn-primary', text: opts.okText || 'Continue', onclick: () => {
                    if (opts.required && !input.value.trim()) { input.focus(); return; }
                    close(input.value);
                } })),
        ];
    });
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

export const reducedMotion = () => window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;

export function debounce(fn, ms) {
    let t = null;
    return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
}

export function field(label, control, help) {
    return h('label', { class: 'field' }, h('span', { class: 'field-label', text: label }), control, help ? h('span', { class: 'field-help', text: help }) : null);
}

export function lines(text) { return String(text || '').split('\n').map(x => x.trim()).filter(Boolean); }

export function codeLines(lines, startLine) {
    return h('pre', { class: 'code' }, lines.map((l, i) => h('span', { class: 'code-line' },
        h('span', { class: 'ln', 'aria-hidden': 'true', text: (l.n !== undefined ? l.n : startLine + i) }), l.text !== undefined ? l.text : l)));
}

export function diffView(diff) {
    const out = [];
    let run = [];
    const flush = () => {
        if (run.length > 8) {
            out.push(...run.slice(0, 3).map(l => h('span', { class: 'diff-line' }, '  ' + l.text)));
            out.push(h('span', { class: 'diff-line diff-skip', text: '  … ' + (run.length - 6) + ' unchanged lines …' }));
            out.push(...run.slice(-3).map(l => h('span', { class: 'diff-line' }, '  ' + l.text)));
        } else out.push(...run.map(l => h('span', { class: 'diff-line' }, '  ' + l.text)));
        run = [];
    };
    for (const l of diff) {
        if (l.op === '=') { run.push(l); continue; }
        flush();
        out.push(h('span', { class: 'diff-line ' + (l.op === '+' ? 'diff-add' : 'diff-del') }, l.op + ' ' + l.text));
    }
    flush();
    return h('pre', { class: 'diff' }, out.length ? out : h('span', { class: 'diff-line diff-skip', text: 'No changes.' }));
}

/// Render a unified git diff as colored lines.
export function unifiedDiff(text) {
    return h('pre', { class: 'diff' }, String(text || '').split('\n').map(l => h('span', {
        class: 'diff-line' + (l.startsWith('+') && !l.startsWith('+++') ? ' diff-add' : l.startsWith('-') && !l.startsWith('---') ? ' diff-del' : l.startsWith('@@') ? ' diff-skip' : ''),
    }, l)));
}

/// Shared mutable app state (shell data, actor).
export const app = { shell: null, listeners: [] };
export function onShell(fn) { app.listeners.push(fn); }
export async function refreshShell() {
    try {
        app.shell = await api('/api/shell');
        app.listeners.forEach(fn => { try { fn(app.shell); } catch (_) { /* ignore */ } });
    } catch (_) { /* shown elsewhere */ }
    return app.shell;
}
