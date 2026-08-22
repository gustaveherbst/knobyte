// MCP: the tools agents use through the Model Context Protocol, grouped by tool profile.

import { h, fill, api, pageHeader, card, badge, kv } from '../core.js';

export async function page(ctx) {
    const o = await api('/api/overview');
    const tools = o.mcpTools || [];
    const p = o.mcpProfile || { name: 'core', source: 'default', profiles: [] };
    let selected = p.name;

    const chips = h('div', { class: 'chip-row', role: 'group', 'aria-label': 'Tool profiles' });
    const list = h('ul', { class: 'tool-list' });
    const title = h('p', { class: 'muted small' });

    function render() {
        const prof = (p.profiles || []).find(x => x.name === selected);
        const names = new Set(prof ? prof.tools : tools.map(t => t.name));
        const shown = tools.filter(t => names.has(t.name));
        title.textContent = shown.length + ' tools in profile ' + selected + (selected === p.name ? ' (active)' : '');
        fill(chips, (p.profiles || []).map(x => {
            const b = h('button', { type: 'button', class: 'chip' + (x.name === selected ? ' active' : ''), 'aria-pressed': String(x.name === selected), title: x.summary, text: x.name + ' · ' + x.tools.length });
            b.addEventListener('click', () => { selected = x.name; render(); });
            return b;
        }));
        fill(list, shown.map(t => h('li', { class: 'tool-item' },
            h('div', { class: 'tool-name mono' }, t.name, ' ', ...(t.profiles || []).filter(n => n !== 'full').map(n => badge(n, n === 'core' ? 'accent' : 'neutral'))),
            h('div', { class: 'muted small', text: t.description }))));
    }

    fill(ctx.main,
        pageHeader('Model Context Protocol', h('span', null, 'Agents use Knobyte through MCP. Start a server with ', h('code', { text: 'knobyte mcp --stdio' }), '; add ', h('code', { text: '--profile <name>' }), ' for more tools.')),
        card('Tool profile', h('div', null,
            kv([
                ['Default for this project', p.name + ' (' + p.toolCount + ' tools)'],
                ['Chosen by', p.source],
            ]),
            p.error ? h('div', { class: 'notice notice-error', text: p.error }) : null,
            h('p', { class: 'muted small', text: 'Precedence: --profile flag, then KNOBYTE_MCP_PROFILE, then mcp.profile in .knobyte/config.json, else core.' }),
            chips)),
        card('Tools', h('div', null, title, list)));
    render();
}
