// MCP: the tools agents use through the Model Context Protocol.

import { h, fill, api, pageHeader, card } from '../core.js';

export async function page(ctx) {
    const o = await api('/api/overview');
    const tools = o.mcpTools || [];
    fill(ctx.main,
        pageHeader('Model Context Protocol', h('span', null, 'Agents use Knobyte through MCP. Start a server with ', h('code', { text: 'knobyte mcp --stdio' }), ' or ', h('code', { text: 'knobyte mcp --sse' }), '.')),
        card(tools.length + ' registered tools', h('ul', { class: 'tool-list' }, tools.map(t => h('li', { class: 'tool-item' }, h('div', { class: 'tool-name mono', text: t.name }), h('div', { class: 'muted small', text: t.description }))))));
}
