// App-wide job lifecycle observer.
//
// Every tab subscribes to /api/jobs/events (one `job` event per state change
// of any job) and relays each snapshot to the other tabs of this Hub over a
// BroadcastChannel (localStorage `storage` events where BroadcastChannel is
// unavailable). When a job starts, the shell (top bar, nav) updates in every
// tab; when one finishes, every tab refreshes its data once. Without an event
// stream (refused, or no EventSource) the observer polls /api/jobs instead.

import { api, stream, refreshShell } from './core.js';

export const CHANNEL_PREFIX = 'knobyte-hub-job-lifecycle-v1';
const STORAGE_KEY = CHANNEL_PREFIX + ':relay';
const MEMORY = 200;
const POLL_MS = 5000;
const ACTIVE = new Set(['queued', 'running']);

export const isActiveJob = job => !!job && ACTIVE.has(job.state);

const states = new Map(); // job id -> last known state (bounded)
let channel = null;
let storageRelay = false;
let started = false;
let refreshTimer = null;
let closeStream = null;
let pollTimer = null;
const listeners = [];

/// Subscribe to lifecycle changes: fn(job, { started, finished, fromPeer }).
export function onJobLifecycle(fn) { listeners.push(fn); }

function remember(id, state) {
    states.delete(id);
    states.set(id, state);
    while (states.size > MEMORY) states.delete(states.keys().next().value);
}

function validSnapshot(job) {
    return job && typeof job === 'object' && typeof job.id === 'string' && job.id.length <= 64
        && typeof job.state === 'string' && typeof job.kind === 'string';
}

/// Fold one job snapshot in; returns what changed.
export function observeJob(job, fromPeer) {
    if (!validSnapshot(job)) return null;
    const known = states.has(job.id);
    const prev = states.get(job.id);
    if (prev === job.state) return null;
    // A terminal job never goes back to active (late or replayed peer messages).
    if (known && !ACTIVE.has(prev) && ACTIVE.has(job.state)) return null;
    remember(job.id, job.state);
    const change = {
        started: ACTIVE.has(job.state) && (!known || !ACTIVE.has(prev)),
        // Only a transition we watched counts as finishing; history seen at boot does not.
        finished: !ACTIVE.has(job.state) && known && ACTIVE.has(prev),
        fromPeer: !!fromPeer,
    };
    if (!fromPeer) publish(job);
    listeners.forEach(fn => { try { fn(job, change); } catch (_) { /* ignore */ } });
    if (change.started) refreshShell();
    if (change.finished) scheduleDataRefresh(job);
    return change;
}

/// Refresh every page's data once (coalesced) after a job finished.
function scheduleDataRefresh(job) {
    if (refreshTimer) clearTimeout(refreshTimer);
    refreshTimer = setTimeout(() => {
        refreshTimer = null;
        document.dispatchEvent(new CustomEvent('hub:data-refresh', { detail: { job } }));
    }, 250);
}

function publish(job) {
    const msg = { schemaVersion: 1, type: 'job_snapshot', job };
    if (channel) {
        try { channel.postMessage(msg); } catch (_) { /* cross-tab relay is an optimization */ }
    } else if (storageRelay) {
        try { localStorage.setItem(STORAGE_KEY, JSON.stringify({ ...msg, nonce: Math.random() })); } catch (_) { /* ignore */ }
    }
}

function receive(data) {
    if (!data || data.schemaVersion !== 1 || data.type !== 'job_snapshot') return;
    observeJob(data.job, true);
}

function openRelay(scope) {
    const name = CHANNEL_PREFIX + ':' + String(scope || location.host).slice(0, 64);
    if (typeof BroadcastChannel === 'function') {
        try {
            channel = new BroadcastChannel(name);
            channel.onmessage = ev => receive(ev.data);
            return 'broadcast';
        } catch (_) { channel = null; }
    }
    try {
        localStorage.getItem(STORAGE_KEY);
        storageRelay = true;
        window.addEventListener('storage', ev => {
            if (ev.key !== STORAGE_KEY || !ev.newValue) return;
            try { receive(JSON.parse(ev.newValue)); } catch (_) { /* ignore */ }
        });
        return 'storage';
    } catch (_) { return 'none'; }
}

async function pollOnce() {
    try {
        const page = await api('/api/jobs?limit=20');
        // Oldest first so a start is seen before a finish of the same job.
        page.items.slice().reverse().forEach(j => observeJob(j, false));
    } catch (_) { /* transient */ }
}

function startPolling() {
    if (pollTimer) return;
    pollTimer = setInterval(() => { if (!document.hidden) pollOnce(); }, POLL_MS);
}

function openStream() {
    if (typeof EventSource !== 'function') { startPolling(); return; }
    let close = null;
    close = stream('/api/jobs/events', {
        jobs: hello => { (hello.states || []).forEach(s => { if (!states.has(s.id)) remember(s.id, s.state); }); if (hello.active) observeJob(hello.active, false); },
        job: observeJob,
        resync: () => pollOnce(),
        error: es => {
            // CLOSED: refused (e.g. too many streams) or the session ended; poll instead.
            if (es.readyState === 2) { if (close) close(); closeStream = null; startPolling(); }
        },
    });
    closeStream = close;
}

/// Start observing (once per tab). Seeds the known states first, so jobs that
/// finished before this tab opened do not trigger a refresh.
export async function startJobLifecycle(scope) {
    if (started) return;
    started = true;
    try {
        const page = await api('/api/jobs?limit=50');
        page.items.forEach(j => remember(j.id, j.state));
    } catch (_) { /* seeded by the stream's hello */ }
    openRelay(scope);
    openStream();
    document.addEventListener('visibilitychange', () => { if (!document.hidden && !closeStream) pollOnce(); });
}

/// Stop observing (tests / sign-out).
export function stopJobLifecycle() {
    if (closeStream) closeStream();
    closeStream = null;
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
    if (channel) { try { channel.close(); } catch (_) { /* ignore */ } }
    channel = null;
    started = false;
}
