//! CLI regression tests from the completeness review: Hub port validation and global
//! `--port/--no-open`, `sync --dry-run` exit code.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use tempfile::{tempdir, TempDir};

use knobyte::config::KnobyteConfig;

fn kb(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KNOBYTE_NO_AGENT_LAUNCH", "1")
        .env("PATH", "/usr/bin:/bin")
        .env_remove("CI")
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn project() -> (TempDir, PathBuf) {
    let d = tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    for args in [vec!["init", "-q"], vec!["config", "user.email", "dev@example.com"], vec!["config", "user.name", "Dev"]] {
        Command::new("git").args(&args).current_dir(&root).env("HOME", &root).output().unwrap();
    }
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    knobyte::setup::run_setup(&config, "code-repo", false).unwrap();
    (d, root)
}

#[test]
fn port_zero_and_out_of_range_ports_are_rejected() {
    let (_d, root) = project();
    for args in [
        vec!["hub", "--port", "0", "--no-open"],
        vec!["--port", "0", "--no-open"],
        vec!["--port", "0", "hub"],
        vec!["hub", "--port", "65536"],
        vec!["mcp", "--port", "0"],
    ] {
        let o = kb(&root, &args);
        assert_eq!(o.status.code(), Some(2), "{:?}: {}", args, text(&o));
        let t = text(&o);
        assert!(t.contains("Expected a positive integer") || t.contains("Expected a TCP port"), "{:?}: {}", args, t);
        assert!(!t.contains(":0/"), "{:?}: {}", args, t);
    }
    // Global options still belong to bare `knobyte` / `knobyte hub` only.
    let o = kb(&root, &["--port", "4555", "check"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}

#[test]
fn global_port_and_no_open_apply_to_hub_like_mex() {
    let (_d, root) = project();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let port_s = port.to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(["--port", &port_s, "--no-open", "hub"])
        .current_dir(&root)
        .env("NO_COLOR", "1")
        .env("HOME", &root)
        .env("KNOBYTE_NO_AGENT_LAUNCH", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let wanted = format!("127.0.0.1:{}", port);
    let mut seen = Vec::new();
    let found = loop {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(line) => {
                let hit = line.contains(&wanted);
                seen.push(line);
                if hit {
                    break true;
                }
            }
            Err(_) => break false,
        }
    };
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    assert!(
        found,
        "hub did not report port {}: {:?} {}",
        port,
        seen,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn sync_dry_run_with_drift_exits_zero_like_mex() {
    let (_d, root) = project();
    let arch = root.join(".knobyte/context/architecture.md");
    let mut doc = fs::read_to_string(&arch).unwrap();
    doc.push_str("\nThe entry point lives in `src/definitely_missing_module.rs`.\n");
    fs::write(&arch, doc).unwrap();
    let check = kb(&root, &["check"]);
    assert_ne!(check.status.code(), Some(0), "fixture must have drift errors: {}", text(&check));
    let o = kb(&root, &["sync", "--dry-run"]);
    let t = text(&o);
    assert!(t.contains("--dry-run"), "{}", t);
    assert_eq!(o.status.code(), Some(0), "{}", t);
}
