//! Static, script-free status snapshot. Used by the MCP HTTP server's
//! `/dashboard` route, which has no Hub API behind it. Every dynamic value is
//! HTML-escaped.

use super::escape_html as esc;
use super::projects::{aggregate_contributors, discover_projects};
use crate::config::KnobyteConfig;
use crate::drift::checker::run_drift_check;
use crate::events::read_events;
use crate::team::activity::list_activity;

const STYLE: &str = "body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;\
background:#090d12;color:#cbd5e1;margin:0;padding:1.5rem;max-width:1100px;margin:0 auto}\
h1,h2{color:#f8fafc}h2{font-size:1.05rem;margin-top:2rem;border-bottom:1px solid #232d3d;padding-bottom:.4rem}\
table{width:100%;border-collapse:collapse;font-size:.85rem}td,th{text-align:left;padding:.45rem .5rem;\
border-bottom:1px solid #232d3d;vertical-align:top;overflow-wrap:anywhere}th{color:#64748b;font-weight:600}\
code,.mono{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}.muted{color:#64748b}\
.ok{color:#34d399}.warn{color:#fbbf24}.bad{color:#f87171}";

fn status_class(status: &str) -> &'static str {
    match status {
        "healthy" => "ok",
        "warning" => "warn",
        _ => "bad",
    }
}

/// Render a read-only HTML snapshot of the current project and fleet.
pub fn render_dashboard_html(config: &KnobyteConfig) -> String {
    let projects = discover_projects(config);
    let contributors = aggregate_contributors(&projects, config);
    let drift = run_drift_check(config);

    let mut out = String::new();
    out.push_str("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    out.push_str(&format!("<title>Knobyte — {}</title><style>{}</style></head><body>", esc(&config.project_name()), STYLE));
    out.push_str(&format!(
        "<h1>Knobyte status: {}</h1><p class=\"muted\">v{} · drift score <strong>{:.0}/100</strong> · {} scaffold files. \
         For the interactive Project Hub run <code>knobyte hub</code>.</p>",
        esc(&config.project_name()),
        esc(crate::version::VERSION),
        drift.score,
        drift.file_count
    ));

    out.push_str("<h2>Fleet</h2><table><tr><th>Project</th><th>Path</th><th>Status</th><th>Drift</th><th>Symbols</th></tr>");
    for p in &projects {
        let drift_txt = if p.available { format!("{:.1}%", p.drift_score) } else { "—".to_string() };
        out.push_str(&format!(
            "<tr><td>{}</td><td class=\"mono\">{}</td><td class=\"{}\">{}</td><td>{}</td><td>{}</td></tr>",
            esc(&p.name),
            esc(&p.path),
            status_class(&p.status),
            esc(&p.status),
            drift_txt,
            p.node_count
        ));
    }
    out.push_str("</table>");

    out.push_str("<h2>Contributors</h2>");
    if contributors.is_empty() {
        out.push_str("<p class=\"muted\">No team members registered. Add one with <code>knobyte member add</code>.</p>");
    } else {
        out.push_str("<table><tr><th>Member</th><th>Decisions</th><th>Discoveries</th><th>Relays</th><th>In flight</th></tr>");
        for c in &contributors {
            out.push_str(&format!(
                "<tr><td>{} <span class=\"muted mono\">@{}</span></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&c.display_name),
                esc(&c.git_alias),
                c.decisions_count,
                c.discoveries_count,
                c.relays_authored,
                esc(c.in_flight_relay.as_deref().unwrap_or("—"))
            ));
        }
        out.push_str("</table>");
    }

    let mut rows: Vec<(String, String, String, String)> = read_events(config)
        .into_iter()
        .rev()
        .take(30)
        .map(|e| (e.timestamp, e.actor.unwrap_or_default(), e.kind, e.summary))
        .collect();
    rows.extend(
        list_activity(config, 30)
            .into_iter()
            .map(|a| (a.timestamp, a.actor, format!("team:{}", a.action), format!("{}: {}", a.entity_title, a.summary))),
    );
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.truncate(30);

    out.push_str("<h2>Recent activity</h2>");
    if rows.is_empty() {
        out.push_str("<p class=\"muted\">No recorded activity yet.</p>");
    } else {
        out.push_str("<table><tr><th>Time</th><th>Actor</th><th>Kind</th><th>Summary</th></tr>");
        for (ts, actor, kind, summary) in &rows {
            out.push_str(&format!(
                "<tr><td class=\"mono\">{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&ts.chars().take(19).collect::<String>().replace('T', " ")),
                esc(actor),
                esc(kind),
                esc(summary)
            ));
        }
        out.push_str("</table>");
    }
    out.push_str("</body></html>");
    out
}
