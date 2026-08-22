//! CLI envelope, exit codes and the preview -> apply file round trip.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;

use knobyte::config::KnobyteConfig;
use knobyte::setup::run_setup;

fn kb(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap()
}

fn json_of(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("not JSON ({}): {}", e, String::from_utf8_lossy(&o.stdout)))
}

#[test]
fn envelopes_exit_codes_and_preview_apply_round_trip() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();

    let o = kb(root, &["member", "add", "alex", "--name", "Alex", "--select", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = kb(root, &["member", "show", "alex", "--json"]);
    let v = json_of(&o);
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["command"], "member.show");
    assert_eq!(v["mode"], "read");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["displayName"], "Alex");

    // Not found -> exit 3 with a problem body.
    let o = kb(root, &["member", "show", "ghost", "--json"]);
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(json_of(&o)["problem"]["code"], "NOT_FOUND");

    // Preview writes nothing; apply consumes the exact envelope file.
    let o = kb(root, &["workstream", "create", "ws1", "Billing", "--goal", "Ship", "--preview", "--json"]);
    assert!(o.status.success());
    let env = json_of(&o);
    assert_eq!(env["mode"], "preview");
    assert!(!config.workstreams_dir().join("ws1.json").exists());
    let file = root.join("preview.json");
    std::fs::write(&file, &o.stdout).unwrap();
    let o = kb(root, &["workstream", "create", "--apply", file.to_str().unwrap(), "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(json_of(&o)["mode"], "apply");
    assert!(config.workstreams_dir().join("ws1.json").exists());
    // Applying with the wrong command is a usage error.
    let o = kb(root, &["workstream", "archive", "--apply", file.to_str().unwrap(), "--json"]);
    assert_eq!(o.status.code(), Some(2));

    // A stale envelope (target changed) -> exit 4.
    let o = kb(root, &["workstream", "update", "ws1", "--summary", "one", "--preview", "--json"]);
    std::fs::write(&file, &o.stdout).unwrap();
    assert!(kb(root, &["workstream", "update", "ws1", "--summary", "two"]).status.success());
    let o = kb(root, &["workstream", "update", "--apply", file.to_str().unwrap(), "--json"]);
    assert_eq!(o.status.code(), Some(4));
    assert_eq!(json_of(&o)["problem"]["code"], "REVISION_CONFLICT");

    // Typed inbox draft, contract and target.
    let o = kb(root, &["inbox", "draft", "save", "--change", "knowledge.create", "--kind", "convention", "--title", "Errors", "--body", "Use thiserror", "--reason", "consistency", "--evidence", "file:src/lib.rs", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let draft_id = json_of(&o)["data"]["result"]["id"].as_str().unwrap().to_string();
    let o = kb(root, &["inbox", "draft", "show", &draft_id, "--json"]);
    assert_eq!(json_of(&o)["data"]["change"]["kind"], "knowledge.create");
    let o = kb(root, &["inbox", "contract", "--action", "inbox.approve", "--json"]);
    assert!(json_of(&o)["data"]["commands"]["inbox.approve"]["request"]["$schema"].is_string());
    let o = kb(root, &["relay", "contract", "--json"]);
    assert!(json_of(&o)["data"]["commands"]["relay.draft.save"].is_object());

    // Proposal list paging and --state validation (usage -> 2).
    let o = kb(root, &["inbox", "proposal", "list", "--state", "nope", "--json"]);
    assert_eq!(o.status.code(), Some(2));

    // Relay draft from sparse JSON.
    let sparse = root.join("relay.json");
    std::fs::write(&sparse, r#"{"summary":"Handoff","completed":["x"],"unresolvedQuestions":["why?"]}"#).unwrap();
    let o = kb(root, &["relay", "draft", "save", "--from", sparse.to_str().unwrap(), "--no-auto-files", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let rid = json_of(&o)["data"]["result"]["id"].as_str().unwrap().to_string();
    let o = kb(root, &["relay", "draft", "show", &rid, "--json"]);
    assert_eq!(json_of(&o)["data"]["unresolvedQuestions"][0], "why?");
    assert!(kb(root, &["relay", "publish", &rid]).status.success());
    let o = kb(root, &["relay", "list", "--perspective", "sent", "--json"]);
    assert_eq!(json_of(&o)["data"]["items"].as_array().unwrap().len(), 1);

    // log --type/--source/--status and timeline bounds/format.
    assert!(kb(root, &["log", "Chose Postgres", "--type", "decision", "--source", "meeting", "--status", "decided"]).status.success());
    let o = kb(root, &["timeline", "--json"]);
    let v = json_of(&o);
    assert_eq!(v["entries"][0]["source"], "meeting");
    assert_eq!(v["entries"][0]["kind"], "decision");
    let o = kb(root, &["timeline", "--format", "md", "--since", "30d"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("| Date | Type | Event | Files |"));
    assert_eq!(kb(root, &["timeline", "--limit", "500"]).status.code(), Some(2));
    assert_eq!(kb(root, &["timeline", "--limit", "0"]).status.code(), Some(2));
}

#[test]
fn contract_subcommands_and_help_descriptions() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();

    for (family, action) in [("member", "member.add"), ("workstream", "workstream.step.update"), ("activity", "activity.record")] {
        let o = kb(root, &[family, "contract", "--json"]);
        assert!(o.status.success(), "{}: {}", family, String::from_utf8_lossy(&o.stderr));
        let v = json_of(&o);
        assert_eq!(v["data"]["family"], family);
        assert!(v["data"]["commands"][action]["request"]["$schema"].is_string(), "{}", v);
        let o = kb(root, &[family, "contract", "--action", action, "--json"]);
        assert_eq!(json_of(&o)["data"]["commands"].as_object().unwrap().len(), 1);
        assert_eq!(kb(root, &[family, "contract", "--action", "nope"]).status.code(), Some(2));
        // The --request help points at a subcommand that exists.
        let help = String::from_utf8_lossy(&kb(root, &[family, "contract", "--help"]).stdout).to_string();
        assert!(help.contains("JSON Schema"), "{}", help);
    }

    // A request file shaped by the member contract is accepted.
    let req = root.join("member.json");
    std::fs::write(&req, r#"{"action":{"kind":"member.add","member":{"id":"sam","displayName":"Sam"}}}"#).unwrap();
    let o = kb(root, &["member", "add", "--request", req.to_str().unwrap(), "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));

    // No blank subcommand descriptions.
    for args in [vec!["relay", "draft", "--help"], vec!["relay", "--help"], vec!["inbox", "--help"]] {
        let help = String::from_utf8_lossy(&kb(root, &args).stdout).to_string();
        let commands = help.split("Commands:").nth(1).unwrap().split("Options:").next().unwrap();
        for line in commands.lines().filter(|l| !l.trim().is_empty()) {
            assert!(line.split_whitespace().count() > 1, "{:?} has a blank description: {:?}", args, line);
        }
    }
}

#[test]
fn member_flag_cannot_override_the_resolved_actor() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();
    assert!(kb(root, &["member", "add", "bob", "--name", "Bob"]).status.success());
    assert!(kb(root, &["member", "add", "alex", "--name", "Alex", "--select"]).status.success());

    let o = kb(root, &["inbox", "draft", "save", "--change", "knowledge.create", "--kind", "convention", "--title", "Errors", "--body", "Use thiserror", "--reason", "consistency", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let draft_id = json_of(&o)["data"]["result"]["id"].as_str().unwrap().to_string();
    let o = kb(root, &["inbox", "publish", &draft_id, "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let pid = json_of(&o)["data"]["result"]["id"].as_str().unwrap().to_string();

    // Approving (or otherwise deciding) your own proposal "as bob" is refused.
    for args in [
        vec!["inbox", "proposal", "approve", pid.as_str(), "--member", "bob", "--json"],
        vec!["inbox", "approve", pid.as_str(), "--member", "bob", "--json"],
        vec!["inbox", "proposal", "reject", pid.as_str(), "--member", "bob", "--json"],
        vec!["inbox", "proposal", "mark-stale", pid.as_str(), "--member", "bob", "--reason", "x", "--json"],
        vec!["inbox", "proposal", "withdraw", pid.as_str(), "--member", "bob", "--json"],
    ] {
        let o = kb(root, &args);
        assert_eq!(o.status.code(), Some(5), "{:?}: {}", args, String::from_utf8_lossy(&o.stdout));
        let v = json_of(&o);
        assert_eq!(v["problem"]["code"], "UNAUTHORIZED");
        assert!(v["problem"]["detail"].as_str().unwrap().starts_with("ACTOR_MISMATCH"), "{}", v);
    }
    let p = json_of(&kb(root, &["inbox", "proposal", "show", &pid, "--json"]));
    assert_eq!(p["data"]["status"], "pending");
    assert!(p["data"].get("decisionBy").map(|d| d.is_null()).unwrap_or(true));

    // The plain self-approval refusal is transport-neutral and carries a code.
    let o = kb(root, &["inbox", "proposal", "approve", &pid, "--json"]);
    assert_eq!(o.status.code(), Some(5));
    let detail = json_of(&o)["problem"]["detail"].as_str().unwrap().to_string();
    assert!(detail.starts_with("SELF_APPROVAL_REQUIRED"), "{}", detail);
    assert!(!detail.contains("--self-approve"), "{}", detail);
    let o = kb(root, &["inbox", "proposal", "approve", &pid]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("--self-approve"));

    // --member equal to the resolved actor is accepted.
    let o = kb(root, &["inbox", "proposal", "approve", &pid, "--member", "alex", "--self-approve", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(json_of(&o)["data"]["result"]["decisionBy"], "alex");

    // Relay drafts and transitions refuse a foreign actor too.
    let o = kb(root, &["relay", "draft", "save", "--title", "Handoff", "--summary", "s", "--no-auto-files", "--sender", "bob", "--json"]);
    assert_eq!(o.status.code(), Some(5), "{}", String::from_utf8_lossy(&o.stdout));
    let o = kb(root, &["relay", "draft", "save", "--title", "Handoff", "--summary", "s", "--no-auto-files", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let rid = json_of(&o)["data"]["result"]["id"].as_str().unwrap().to_string();
    assert_eq!(kb(root, &["relay", "publish", &rid, "--member", "bob", "--json"]).status.code(), Some(5));
    let o = kb(root, &["relay", "publish", &rid, "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let relay_id = json_of(&o)["data"]["result"]["id"].as_str().unwrap().to_string();
    assert_eq!(kb(root, &["relay", "acknowledge", &relay_id, "--member", "bob", "--json"]).status.code(), Some(5));
}
