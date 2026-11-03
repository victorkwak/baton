//! Notes sync end to end: the real binary, real git, two clones of a local
//! bare repo standing in for two machines.

#[path = "common/notes.rs"]
mod common;

use std::fs;

use common::*;

#[test]
fn new_file_reaches_the_other_clone() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);

    ws.write("ideas/new.md", "hello\n");
    let out = ws.agent(&["sync", "--paths", "ideas/new.md", "-m", "add idea"]);
    ok(&out);
    let text = stdout(&out);
    assert!(text.starts_with("# step\tsha\tpaths\n"), "{text}");
    assert!(
        text.contains("committed\t") && text.contains("ideas/new.md"),
        "{text}"
    );
    assert!(text.contains("pushed\t"), "{text}");
    assert_eq!(ws.remote_main(), ws.head());
    let msg = ws.git(&["log", "-1", "--format=%B"]);
    assert!(msg.starts_with("add idea"), "{msg}");
    assert!(msg.contains("Baton-Machine: ws"), "{msg}");
    assert!(msg.contains("Baton-Session: s1"), "{msg}");

    // The other machine pulls with a sync that has nothing of its own.
    ok(&lap.agent(&["sync", "--paths", "ideas/new.md"]));
    assert_eq!(lap.read("ideas/new.md"), "hello\n");
    assert_eq!(lap.head(), ws.head());
}

#[test]
fn absolute_paths_and_json_output() {
    let w = World::new();
    let ws = w.machine("ws", false);
    ws.write("a.md", "a\n");
    let abs = ws.repo.join("a.md");
    let out = ws.agent(&["--format", "json", "sync", "--paths", abs.to_str().unwrap()]);
    ok(&out);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "sync");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items[0]["step"], "committed");
    assert_eq!(items[0]["paths"], serde_json::json!(["a.md"]));
    assert_eq!(items.last().unwrap()["step"], "pushed");

    // Paths outside the repo are refused.
    let out = ws.agent(&["sync", "--paths", "../x.md"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    let out = ws.agent(&["sync", "--paths", "."]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
}

#[test]
fn nothing_to_do_prints_nothing() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    ok(&out);
    assert_eq!(stdout(&out), "");
}

#[test]
fn other_sessions_edits_stay_uncommitted() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);

    // Another session's unstaged edit, staged file, and untracked file.
    ws.write(
        "note.md",
        "one\ntwo\nthree\nfour\nfive\nsix (other session)\n",
    );
    ws.write("staged.md", "staged by someone else\n");
    ws.git(&["add", "staged.md"]);
    ws.write("scratch.md", "untracked, not mine\n");
    ws.write("mine.md", "mine\n");

    // An incoming change elsewhere, so the apply step runs too.
    lap.write("from-laptop.md", "hi\n");
    ok(&lap.agent(&["sync", "--paths", "from-laptop.md"]));

    let out = ws.agent(&["sync", "--paths", "mine.md"]);
    ok(&out);
    let text = stdout(&out);
    assert!(text.contains("integrated\t"), "{text}");
    // The staged entry was reset by `reset --keep` and is reported.
    assert!(text.contains("unstaged\t\tstaged.md"), "{text}");

    let committed: Vec<String> = ws
        .git(&["log", "--format=%H", "origin/main", "--author=ws"])
        .lines()
        .flat_map(|sha| ws.files_in(sha))
        .collect();
    assert_eq!(committed, vec!["mine.md"]);
    assert_eq!(
        ws.read("note.md"),
        "one\ntwo\nthree\nfour\nfive\nsix (other session)\n"
    );
    assert_eq!(ws.read("staged.md"), "staged by someone else\n");
    assert_eq!(ws.read("scratch.md"), "untracked, not mine\n");
    assert_eq!(ws.read("from-laptop.md"), "hi\n");
    let status = ws.status();
    assert!(status.contains(&" M note.md".to_owned()), "{status:?}");
    assert!(status.contains(&"?? staged.md".to_owned()), "{status:?}");
    assert!(status.contains(&"?? scratch.md".to_owned()), "{status:?}");
    assert_eq!(status.len(), 3, "{status:?}");

    // A snapshot of the uncommitted work was taken.
    let backups = ws.git(&["for-each-ref", "--format=%(refname)", "refs/baton/backup/"]);
    assert!(!backups.is_empty());
    let copies = fs::read_dir(ws.git_dir().join("baton/backup"))
        .unwrap()
        .count();
    assert_eq!(copies, 1);
}

#[test]
fn untracked_file_at_incoming_path_collides() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);

    lap.write("shared.md", "from laptop\n");
    ok(&lap.agent(&["sync", "--paths", "shared.md"]));

    ws.write("shared.md", "ws draft, untracked\n");
    let before = ws.head();
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    assert_eq!(code(&out), 20, "{}", stderr(&out));
    assert!(stderr(&out).contains("shared.md"), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim().lines().count(), 1, "{}", stderr(&out));
    assert_eq!(ws.head(), before);
    assert_eq!(ws.read("shared.md"), "ws draft, untracked\n");
    assert_eq!(ws.status(), vec!["?? shared.md".to_owned()]);
    assert!(!ws.git_dir().join("baton/journal.json").exists());
}

#[test]
fn ignored_file_at_incoming_path_collides() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);

    // `*.tmp` is ignored, but laptop force-adds one.
    lap.write("cache.tmp", "laptop\n");
    lap.git(&["add", "-f", "cache.tmp"]);
    lap.git(&["commit", "-q", "-m", "force-add"]);
    lap.git(&["push", "-q", "origin", "main"]);

    ws.write("cache.tmp", "ws ignored file\n");
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    assert_eq!(code(&out), 20, "{}", stderr(&out));
    assert!(stderr(&out).contains("cache.tmp"));
    assert_eq!(ws.read("cache.tmp"), "ws ignored file\n");
}

#[test]
fn dirty_file_changed_upstream_collides() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);

    lap.write("note.md", "one\ntwo\nthree\nfour\nFIVE\n");
    ok(&lap.agent(&["sync", "--paths", "note.md"]));

    ws.write(
        "note.md",
        "ONE (uncommitted, other session)\ntwo\nthree\nfour\nfive\n",
    );
    ws.write("mine.md", "mine\n");
    let out = ws.agent(&["sync", "--paths", "mine.md"]);
    assert_eq!(code(&out), 20, "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("note.md") && err.contains("kept"), "{err}");
    // The local commit stays; nothing applied or pushed.
    assert_eq!(ws.files_in("HEAD"), vec!["mine.md"]);
    assert!(stdout(&out).contains("committed\t"));
    assert_eq!(
        ws.read("note.md"),
        "ONE (uncommitted, other session)\ntwo\nthree\nfour\nfive\n"
    );
    assert_ne!(ws.remote_main(), ws.head());

    // Once the edit is committed, a sync integrates (no overlap: line 1 vs 5).
    ok(&ws.agent(&["sync", "--paths", "note.md"]));
    assert_eq!(
        ws.read("note.md"),
        "ONE (uncommitted, other session)\ntwo\nthree\nfour\nFIVE\n"
    );
    assert_eq!(ws.remote_main(), ws.head());
}

#[test]
fn adjacent_line_conflict_exits_21_without_markers() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);

    lap.write("note.md", "one\ntwo\nTHREE laptop\nfour\nfive\n");
    ok(&lap.agent(&["sync", "--paths", "note.md"]));

    ws.write("note.md", "one\nTWO ws\nthree\nfour\nfive\n");
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    assert_eq!(code(&out), 21, "{}", stderr(&out));
    assert!(stderr(&out).contains("note.md"), "{}", stderr(&out));
    assert_no_markers(&ws.repo);
    assert_eq!(ws.read("note.md"), "one\nTWO ws\nthree\nfour\nfive\n");
    // The local commit stays, the tree is clean, no rebase is in progress.
    assert_eq!(ws.files_in("HEAD"), vec!["note.md"]);
    assert!(ws.status().is_empty(), "{:?}", ws.status());
    assert!(!ws.git_dir().join("rebase-merge").exists());
    let wt = ws.git_dir().join("baton/wt");
    let wt_status = git(&wt, &["status", "--porcelain"]);
    assert!(wt_status.is_empty(), "{wt_status}");
    assert!(!ws.git_dir().join("worktrees/wt/rebase-merge").exists());
    assert_ne!(ws.remote_main(), ws.head());
}

#[test]
fn failing_check_exits_22_and_keeps_the_commit() {
    let w = World::new();
    let ws = w.machine("ws", true);
    ws.write("bad.md", "this is BAD\n");
    let out = ws.agent(&["sync", "--paths", "bad.md"]);
    assert_eq!(code(&out), 22, "{}", stderr(&out));
    assert!(stderr(&out).contains("BAD found"), "{}", stderr(&out));
    assert_eq!(ws.files_in("HEAD"), vec!["bad.md"]);
    assert_ne!(ws.remote_main(), ws.head());

    // Fixed in a second commit, the check passes and both are pushed.
    ws.write("bad.md", "this is fine\n");
    ok(&ws.agent(&["sync", "--paths", "bad.md"]));
    assert_eq!(ws.remote_main(), ws.head());
}

#[test]
fn check_runs_on_the_merged_tree() {
    let w = World::new();
    let ws = w.machine("ws", true);
    // Laptop has no check and pushes a BAD file.
    let lap = w.machine("laptop", false);
    lap.write("upstream.md", "BAD upstream\n");
    ok(&lap.agent(&["sync", "--paths", "upstream.md"]));

    ws.write("fine.md", "fine\n");
    let out = ws.agent(&["sync", "--paths", "fine.md"]);
    assert_eq!(code(&out), 22, "{}", stderr(&out));
    assert!(!ws.exists("upstream.md"), "nothing applied");
}

#[test]
fn pre_commit_hook_failure_exits_22() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let hook = ws.git_dir().join("hooks/pre-commit");
    fs::write(&hook, "#!/bin/sh\necho 'hook says no' >&2\nexit 1\n").unwrap();
    make_executable(&hook);

    ws.write("new.md", "x\n");
    let before = ws.head();
    let out = ws.agent(&["sync", "--paths", "new.md"]);
    assert_eq!(code(&out), 22, "{}", stderr(&out));
    assert!(stderr(&out).contains("hook says no"), "{}", stderr(&out));
    assert_eq!(ws.head(), before);
    // The intent-to-add entry is undone: the file is plain untracked again.
    assert_eq!(ws.status(), vec!["?? new.md".to_owned()]);
}

#[test]
fn push_race_with_another_clone_resolves() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);
    // Deterministic race: laptop pushes while ws is pushing (ws's pre-push hook).
    let hook = ws.git_dir().join("hooks/pre-push");
    let marker = w.tmp.path().join("raced");
    let script = format!(
        "#!/bin/sh\nif [ ! -e {m} ]; then touch {m}; cd {lap} && echo race > race.md && git add race.md && git commit -qm race && git push -q origin main; fi\nexit 0\n",
        m = marker.display(),
        lap = lap.repo.display()
    );
    fs::write(&hook, script).unwrap();
    make_executable(&hook);

    ws.write("mine.md", "mine\n");
    let out = ws.agent(&["sync", "--paths", "mine.md"]);
    ok(&out);
    assert!(marker.exists(), "the race happened");
    assert_eq!(ws.remote_main(), ws.head());
    assert_eq!(ws.read("race.md"), "race\n");
    // Linear history: the race commit, then ws's rebased commit.
    let log = ws.git(&["log", "--format=%s", "-2"]);
    assert_eq!(log.lines().nth(1), Some("race"), "{log}");
    assert_eq!(ws.git(&["rev-list", "--merges", "--count", "HEAD"]), "0");
}

#[test]
fn concurrent_syncs_from_two_clones_lose_nothing() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);
    const N: usize = 8;
    std::thread::scope(|s| {
        for m in [&ws, &lap] {
            s.spawn(move || {
                for i in 0..N {
                    let name = format!("{}-{i}.md", m.name);
                    m.write(&name, "x\n");
                    let out = m.agent(&["sync", "--paths", &name]);
                    // A tight loop can starve one side past its retries:
                    // exit 32 with the local commit kept is the designed outcome.
                    match code(&out) {
                        0 => {}
                        32 => assert!(stderr(&out).contains("kept"), "{}", stderr(&out)),
                        _ => ok(&out),
                    }
                }
            });
        }
    });
    // Settle: each side pushes what it kept, then ws pulls the rest.
    ok(&ws.agent(&["sync", "--paths", "note.md"]));
    ok(&lap.agent(&["sync", "--paths", "note.md"]));
    ok(&ws.agent(&["sync", "--paths", "note.md"]));
    for m in [&ws, &lap] {
        for who in ["ws", "laptop"] {
            for i in 0..N {
                assert!(
                    m.exists(&format!("{who}-{i}.md")),
                    "{} lacks {who}-{i}",
                    m.name
                );
            }
        }
        assert_eq!(m.head(), m.remote_main());
    }
    assert_eq!(ws.git(&["rev-list", "--merges", "--count", "HEAD"]), "0");
}

#[test]
fn interrupted_apply_is_finished_from_the_journal() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);
    lap.write("incoming.md", "incoming\n");
    ok(&lap.agent(&["sync", "--paths", "incoming.md"]));

    // Crash after the journal is written, before the reset.
    ws.write("mine.md", "mine\n");
    let out = ws.baton_env(
        &["sync", "--paths", "mine.md"],
        &[("CLAUDECODE", "1"), ("BATON_FAULT", "after-journal")],
    );
    assert_eq!(code(&out), 99, "{}", stderr(&out));
    let journal = ws.git_dir().join("baton/journal.json");
    assert!(journal.exists());
    assert!(!ws.exists("incoming.md"));
    // As if the killed reset had already written one file.
    ws.write("incoming.md", "incoming\n");

    let out = ws.agent(&["sync", "--paths", "note.md"]);
    ok(&out);
    assert!(stdout(&out).contains("recovered\t"), "{}", stdout(&out));
    assert!(!journal.exists());
    assert!(ws.status().is_empty(), "{:?}", ws.status());
    assert_eq!(ws.read("incoming.md"), "incoming\n");
    assert_eq!(ws.head(), ws.remote_main());
    assert!(ws.exists("mine.md"));
}

#[test]
fn crash_after_reset_is_finished_from_the_journal() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);
    lap.write("incoming.md", "incoming\n");
    ok(&lap.agent(&["sync", "--paths", "incoming.md"]));

    ws.write("mine.md", "mine\n");
    let out = ws.baton_env(
        &["sync", "--paths", "mine.md"],
        &[("CLAUDECODE", "1"), ("BATON_FAULT", "after-reset")],
    );
    assert_eq!(code(&out), 99);
    assert!(ws.git_dir().join("baton/journal.json").exists());
    assert!(ws.exists("incoming.md"));
    assert_ne!(ws.head(), ws.remote_main(), "not pushed yet");

    let out = ws.agent(&["sync", "--paths", "note.md"]);
    ok(&out);
    let text = stdout(&out);
    assert!(
        text.contains("recovered\t") && text.contains("pushed\t"),
        "{text}"
    );
    assert_eq!(ws.head(), ws.remote_main());
}

#[test]
fn journal_with_unexpected_head_needs_the_human() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);
    lap.write("incoming.md", "incoming\n");
    ok(&lap.agent(&["sync", "--paths", "incoming.md"]));
    ws.write("mine.md", "mine\n");
    let out = ws.baton_env(
        &["sync", "--paths", "mine.md"],
        &[("CLAUDECODE", "1"), ("BATON_FAULT", "after-journal")],
    );
    assert_eq!(code(&out), 99);
    // The human commits by hand meanwhile.
    ws.write("hand.md", "x\n");
    ws.git(&["add", "hand.md"]);
    ws.git(&["commit", "-q", "-m", "by hand"]);
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    assert_eq!(code(&out), 40, "{}", stderr(&out));
    assert!(stderr(&out).contains("journal"), "{}", stderr(&out));
}

#[test]
fn all_is_refused_for_agents() {
    let w = World::new();
    let ws = w.machine("ws", false);
    ws.write("a.md", "a\n");
    let out = ws.agent(&["sync", "--all"]);
    assert_eq!(code(&out), 10);
    assert!(stderr(&out).contains("--paths"), "{}", stderr(&out));
    assert!(ws.status().contains(&"?? a.md".to_owned()));
    let out = ws.agent(&["sweep"]);
    assert_eq!(code(&out), 10);

    // The human (or a script) may.
    ws.write("b.md", "b\n");
    let out = ws.baton(&["sync", "--all"]);
    ok(&out);
    assert!(ws.status().is_empty());
    let msg = ws.git(&["log", "-1", "--format=%B"]);
    assert!(msg.contains("Baton-Session: cli"), "{msg}");
    assert_eq!(ws.remote_main(), ws.head());
}

#[test]
fn preflight_refuses_bad_repo_states() {
    let w = World::new();
    let ws = w.machine("ws", false);

    ws.git(&["switch", "-q", "-c", "side"]);
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    assert_eq!(code(&out), 40, "{}", stderr(&out));
    assert!(stderr(&out).contains("not on branch main"));
    ws.git(&["switch", "-q", "main"]);

    fs::create_dir(ws.git_dir().join("rebase-merge")).unwrap();
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    assert_eq!(code(&out), 40);
    assert!(stderr(&out).contains("rebase"));
    fs::remove_dir(ws.git_dir().join("rebase-merge")).unwrap();

    fs::write(ws.git_dir().join("MERGE_HEAD"), ws.head()).unwrap();
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    assert_eq!(code(&out), 40);
    fs::remove_file(ws.git_dir().join("MERGE_HEAD")).unwrap();

    // A shallow clone.
    let shallow = w.tmp.path().join("shallow");
    let url = format!("file://{}", w.bare.display());
    git(
        w.tmp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            shallow.to_str().unwrap(),
        ],
    );
    let cfg = ws.dir.join("config/config.toml");
    let text = fs::read_to_string(&cfg).unwrap();
    fs::write(
        &cfg,
        text.replace(ws.repo.to_str().unwrap(), shallow.to_str().unwrap()),
    )
    .unwrap();
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    assert_eq!(code(&out), 40, "{}", stderr(&out));
    assert!(stderr(&out).contains("shallow"));
}

#[test]
fn unrelated_history_needs_the_human() {
    let w = World::new();
    let ws = w.machine("ws", false);
    ws.git(&["checkout", "-q", "--orphan", "fresh"]);
    ws.git(&["commit", "-q", "-m", "orphan"]);
    ws.git(&["branch", "-M", "fresh", "main"]);
    let out = ws.agent(&["sync", "--paths", "note.md"]);
    assert_eq!(code(&out), 40, "{}", stderr(&out));
    assert!(stderr(&out).contains("unrelated"));
}

#[test]
fn fetch_failure_keeps_the_local_commit() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let missing = w.tmp.path().join("gone.git");
    ws.git(&["remote", "set-url", "origin", missing.to_str().unwrap()]);
    ws.write("mine.md", "mine\n");
    let out = ws.agent(&["sync", "--paths", "mine.md"]);
    assert_ne!(code(&out), 0);
    assert!(stderr(&out).contains("kept"), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim().lines().count(), 1, "{}", stderr(&out));
    assert_eq!(ws.files_in("HEAD"), vec!["mine.md"]);
}

#[test]
fn sweep_commits_after_idle_and_leaves_out_failing_files() {
    let w = World::new();
    let ws = w.machine("ws", true);
    let lap = w.machine("laptop", true);
    ws.write("good.md", "good\n");
    ws.write("bad.md", "BAD\n");

    // Recently modified: not idle, nothing happens.
    let out = ws.baton_env(&["sweep"], &[("BATON_SWEEP_IDLE_SECS", "300")]);
    ok(&out);
    assert_eq!(stdout(&out), "");
    assert_eq!(ws.status().len(), 2);

    let out = ws.baton_env(&["sweep"], &[("BATON_SWEEP_IDLE_SECS", "0")]);
    assert_eq!(code(&out), 22, "{}", stderr(&out));
    assert!(stderr(&out).contains("bad.md"), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("left-out\t\tbad.md"), "{text}");
    assert!(
        text.contains("committed\t") && text.contains("pushed\t"),
        "{text}"
    );
    assert_eq!(ws.status(), vec!["?? bad.md".to_owned()]);
    assert_eq!(ws.files_in("HEAD"), vec!["good.md"]);
    let msg = ws.git(&["log", "-1", "--format=%B"]);
    assert!(msg.contains("Baton-Session: sweep"), "{msg}");
    assert_eq!(ws.remote_main(), ws.head());

    // The other machine's sweep pulls while it is clean.
    let out = lap.baton_env(&["sweep"], &[("BATON_SWEEP_IDLE_SECS", "0")]);
    ok(&out);
    assert_eq!(lap.read("good.md"), "good\n");
    assert!(!lap.exists("bad.md"));
}

#[test]
fn sweep_skips_while_the_lock_is_held() {
    let w = World::new();
    let ws = w.machine("ws", false);
    ws.write("a.md", "a\n");
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(ws.git_dir().join("baton.lock"))
        .unwrap();
    lock.lock().unwrap();
    let out = ws.baton_env(&["sweep"], &[("BATON_SWEEP_IDLE_SECS", "0")]);
    ok(&out);
    assert_eq!(stdout(&out), "");
    assert_eq!(ws.status(), vec!["?? a.md".to_owned()]);
    drop(lock);
    ok(&ws.baton_env(&["sweep"], &[("BATON_SWEEP_IDLE_SECS", "0")]));
    assert!(ws.status().is_empty());
}

#[test]
fn deletions_and_directories_sync() {
    let w = World::new();
    let ws = w.machine("ws", false);
    let lap = w.machine("laptop", false);
    ws.write("dir/a.md", "a\n");
    ws.write("dir/sub/b.md", "b\n");
    ok(&ws.agent(&["sync", "--paths", "dir"]));
    fs::remove_file(ws.repo.join("README.md")).unwrap();
    ok(&ws.agent(&["sync", "--paths", "README.md"]));

    ok(&lap.agent(&["sync", "--paths", "dir"]));
    assert!(lap.exists("dir/sub/b.md"));
    assert!(!lap.exists("README.md"));
}
