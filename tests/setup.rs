//! End-to-end tests for `baton init` and `baton doctor`: the real binary,
//! real git, temporary directories for everything (`HOME` included).

#[path = "common/board.rs"]
mod common;

use serde_json::Value;

use common::{Machine, Net, git};

/// A machine with no config yet.
fn fresh(net: &Net, name: &str) -> Machine {
    let m = net.machine(name, &["ws", "laptop"]);
    std::fs::remove_file(m.config_dir().join("config.toml")).unwrap();
    m
}

fn init_args<'a>(net_url: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec![
        "init",
        "--machine",
        "ws",
        "--machines",
        "ws,laptop",
        "--human",
        "victor",
        "--board",
        net_url,
    ];
    a.extend_from_slice(extra);
    a
}

/// The first JSON object printed (the settings block).
fn settings_block(out: &str) -> Value {
    let start = out.find("\n{").expect("settings JSON") + 1;
    let mut depth = 0;
    let mut end = start;
    for (i, c) in out[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    serde_json::from_str(&out[start..end]).unwrap()
}

#[test]
fn init_writes_config_clones_and_prints_settings() {
    let net = Net::new();
    let m = fresh(&net, "ws");
    let url = net.url();
    let out = m.run(&init_args(&url, &[])).ok();
    let cfg = std::fs::read_to_string(m.config_dir().join("config.toml")).unwrap();
    assert!(
        cfg.contains("machine = \"ws\"") && cfg.contains(&url),
        "{cfg}"
    );
    assert!(m.clone_dir().join(".git").exists());

    let s = settings_block(&out);
    let stop = &s["hooks"]["Stop"][0]["hooks"][0];
    assert_eq!(stop["asyncRewake"], true);
    assert_eq!(stop["timeout"], 3600);
    assert!(stop["command"].as_str().unwrap().ends_with(" hook wait"));
    assert!(s["hooks"].get("PreToolUse").is_none(), "{s}");
    let deny = s["permissions"]["deny"][0].as_str().unwrap();
    assert!(
        deny.starts_with("Edit(//") && deny.ends_with("/board/**)"),
        "{deny}"
    );
    assert!(!out.contains("SessionStart"));
    // No notes repo: no sweeper.
    assert!(!out.contains("sweep"));
    assert!(!m.config_dir().join("units").exists());
    // Nothing outside baton's directories: HOME holds only what the test made.
    let home: Vec<String> = std::fs::read_dir(&m.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !home.iter().any(|n| n == ".claude" || n == ".config"),
        "{home:?}"
    );

    // The machine works: a post reaches the board.
    m.post("laptop", "hello", &[]);
    assert_eq!(net.history().len(), 1);
}

#[test]
fn init_refuses_to_overwrite_and_validates() {
    let net = Net::new();
    let m = fresh(&net, "ws");
    let url = net.url();
    m.run(&init_args(&url, &[])).ok();
    let path = m.config_dir().join("config.toml");
    let before = std::fs::read_to_string(&path).unwrap();
    let err = m
        .run(&[
            "init",
            "--machine",
            "laptop",
            "--machines",
            "ws,laptop",
            "--human",
            "victor",
            "--board",
            &url,
        ])
        .code(1);
    assert!(err.contains("never overwrites"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

    // Invalid names: nothing written, nothing cloned.
    let m2 = fresh(&net, "laptop");
    let err = m2
        .run(&[
            "init",
            "--machine",
            "Lap",
            "--machines",
            "Lap",
            "--human",
            "victor",
            "--board",
            &url,
        ])
        .code(1);
    assert!(err.contains("invalid name"), "{err}");
    assert!(!m2.config_dir().join("config.toml").exists());
    assert!(!m2.clone_dir().exists());
    // An unreachable board: no config left behind.
    let err = m2.run(&init_args("/nonexistent/board.git", &[])).code(1);
    assert!(!err.is_empty());
    assert!(!m2.config_dir().join("config.toml").exists());
}

#[test]
fn init_with_notes_writes_sweeper_units() {
    let net = Net::new();
    let url = net.url();
    let notes = net.dir.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    git(&notes, &["init", "--quiet", "-b", "main"]);
    let n = notes.to_str().unwrap();

    let m = fresh(&net, "ws");
    let out = m
        .run(&init_args(
            &url,
            &["--notes", n, "--notes-check", "true", "--os", "linux"],
        ))
        .ok();
    let units = m.config_dir().join("units");
    let service = std::fs::read_to_string(units.join("baton-sweep.service")).unwrap();
    let timer = std::fs::read_to_string(units.join("baton-sweep.timer")).unwrap();
    assert!(
        service.contains("Type=oneshot") && service.contains(" sweep\n"),
        "{service}"
    );
    assert!(
        service.contains("Environment=\"BATON_CONFIG_DIR="),
        "{service}"
    );
    assert!(timer.contains("OnUnitActiveSec=1min") && timer.contains("WantedBy=timers.target"));
    assert!(
        out.contains("systemctl --user enable --now baton-sweep.timer"),
        "{out}"
    );
    let cfg = std::fs::read_to_string(m.config_dir().join("config.toml")).unwrap();
    assert!(
        cfg.contains("[notes]") && cfg.contains("check = [\"true\"]"),
        "{cfg}"
    );

    let mac = fresh(&net, "laptop");
    let out = mac
        .run(&[
            "init",
            "--machine",
            "laptop",
            "--machines",
            "ws,laptop",
            "--human",
            "victor",
            "--board",
            &url,
            "--notes",
            n,
            "--os",
            "macos",
        ])
        .ok();
    let plist =
        std::fs::read_to_string(mac.config_dir().join("units/dev.baton.sweep.plist")).unwrap();
    assert!(
        plist.contains("<key>StartInterval</key><integer>60</integer>"),
        "{plist}"
    );
    assert!(plist.contains("<string>sweep</string>"));
    assert!(out.contains("launchctl bootstrap gui/$(id -u)"), "{out}");

    // A notes path that is not a git checkout is refused.
    let m3 = net.machine("x", &["x"]);
    std::fs::remove_file(m3.config_dir().join("config.toml")).unwrap();
    let err = m3
        .run(&[
            "init",
            "--machine",
            "x",
            "--machines",
            "x",
            "--human",
            "victor",
            "--board",
            &url,
            "--notes",
            "/nonexistent",
        ])
        .code(1);
    assert!(err.contains("not a git checkout"), "{err}");
}

/// Doctor's rows as `(status, check, detail)`.
fn doctor_rows(out: &str) -> Vec<(String, String, String)> {
    assert!(out.starts_with("# status\tcheck\tdetail\n"), "{out}");
    common::rows(out)
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone(), r[2].clone()))
        .collect()
}

fn status_of<'a>(rows: &'a [(String, String, String)], check: &str) -> &'a str {
    &rows
        .iter()
        .find(|r| r.1 == check)
        .unwrap_or_else(|| panic!("no {check} in {rows:?}"))
        .0
}

#[test]
fn doctor_reports_each_check() {
    let net = Net::new();
    let url = net.url();
    let notes = net.dir.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    git(&notes, &["init", "--quiet", "-b", "main"]);
    let m = fresh(&net, "ws");
    let out = m
        .run(&init_args(
            &url,
            &["--notes", notes.to_str().unwrap(), "--os", "linux"],
        ))
        .ok();

    // No Claude settings yet: warnings, exit 0.
    let r = m.run(&["doctor"]);
    assert_eq!(r.code, 0, "{r:?}");
    let rows = doctor_rows(&r.stdout);
    for check in [
        "config",
        "board-clone",
        "board-remote",
        "ssh-agent",
        "board-locks",
        "notes",
        "notes-journal",
        "notes-lock",
    ] {
        assert_eq!(status_of(&rows, check), "ok", "{check}: {rows:?}");
    }
    assert_eq!(status_of(&rows, "hooks"), "warn");

    // With the printed settings installed: all ok.
    let claude = m.root.join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("settings.json"),
        settings_block(&out).to_string(),
    )
    .unwrap();
    let rows = doctor_rows(&m.run(&["doctor"]).ok());
    assert_eq!(status_of(&rows, "hook-wait"), "ok", "{rows:?}");
    assert!(rows.iter().all(|r| r.0 == "ok"), "{rows:?}");

    // A leftover guard hook from before its removal warns.
    let mut s = settings_block(&out);
    s["hooks"]["PreToolUse"] = serde_json::json!([{
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "baton hook guard" }]
    }]);
    std::fs::write(claude.join("settings.json"), s.to_string()).unwrap();
    let rows = doctor_rows(&m.run(&["doctor"]).ok());
    assert_eq!(status_of(&rows, "hook-guard"), "warn", "{rows:?}");

    // A Stop hook without asyncRewake fails; an interrupted sync warns.
    let mut s = settings_block(&out);
    s["hooks"]["Stop"][0]["hooks"][0]["asyncRewake"] = Value::Bool(false);
    std::fs::write(claude.join("settings.json"), s.to_string()).unwrap();
    std::fs::create_dir_all(notes.join(".git/baton")).unwrap();
    std::fs::write(notes.join(".git/baton/journal.json"), "{}").unwrap();
    let r = m.run(&["doctor", "--format", "json"]);
    assert_eq!(r.code, 1, "{r:?}");
    let v = r.json();
    let find = |c: &str| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["check"] == c)
            .unwrap()["status"]
            .clone()
    };
    assert_eq!(find("hook-wait"), "fail");
    assert_eq!(find("notes-journal"), "warn");

    // A missing clone and an unreachable remote fail; doctor never clones.
    std::fs::remove_dir_all(m.clone_dir()).unwrap();
    let r = m.run(&["doctor"]);
    assert_eq!(r.code, 1);
    let rows = doctor_rows(&r.stdout);
    assert_eq!(status_of(&rows, "board-clone"), "fail");
    assert!(!m.clone_dir().exists());

    // No config: fail, and the hooks are still checked.
    std::fs::remove_file(m.config_dir().join("config.toml")).unwrap();
    let r = m.run(&["doctor"]);
    assert_eq!(r.code, 1);
    let rows = doctor_rows(&r.stdout);
    assert_eq!(status_of(&rows, "config"), "fail");
    assert_eq!(status_of(&rows, "hook-wait"), "fail");
}
