use tempfile::tempdir;
use knobyte::config::KnobyteConfig;
use knobyte::events::{append_event, query_timeline, TimelineFilter};

#[test]
fn test_events_and_timeline() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let scaffold = root.join(".knobyte");

    let config = KnobyteConfig::new(root, scaffold);

    let entry = append_event(
        &config,
        "Adopted SQLite WAL mode for high concurrency",
        "decision",
        &["database".to_string(), "concurrency".to_string()],
        &["src/db.rs".to_string()],
        Some("Alex"),
    ).unwrap();

    assert_eq!(entry.kind, "decision");
    assert_eq!(entry.actor, Some("Alex".to_string()));

    // Query timeline
    let filter = TimelineFilter {
        query: Some("concurrency".to_string()),
        kind: Some("decision".to_string()),
        file: None,
        since: None,
        include_superseded: false,
        limit: 10,
    };

    let timeline = query_timeline(&config, filter);
    assert_eq!(timeline.total_matched, 1);
    assert_eq!(timeline.entries[0].summary, "Adopted SQLite WAL mode for high concurrency");

    // Supersede old event
    let new_entry = append_event(
        &config,
        "Migrated to TigerBeetle for balance engine",
        "decision",
        &["database".to_string()],
        &["src/ledger.rs".to_string()],
        Some("Alex"),
    ).unwrap();

    let superseded = knobyte::events::supersede_event(&config, &entry.id, &new_entry.id).unwrap();
    assert!(superseded);

    // Query timeline with default (include_superseded: false)
    let filter2 = TimelineFilter {
        query: None,
        kind: Some("decision".to_string()),
        file: None,
        since: None,
        include_superseded: false,
        limit: 10,
    };
    let timeline2 = query_timeline(&config, filter2);
    assert_eq!(timeline2.total_matched, 1);
    assert_eq!(timeline2.entries[0].summary, "Migrated to TigerBeetle for balance engine");

    // Query timeline with include_superseded: true
    let filter3 = TimelineFilter {
        query: None,
        kind: Some("decision".to_string()),
        file: None,
        since: None,
        include_superseded: true,
        limit: 10,
    };
    let timeline3 = query_timeline(&config, filter3);
    assert_eq!(timeline3.total_matched, 2);
}
