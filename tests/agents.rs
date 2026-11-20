//! End-to-end tests with a real tmux server and a fake CLI.

mod common;

use std::time::{Duration, Instant};

use common::Env;

fn state(env: &Env, name: &str) -> String {
    env.field(name, "state")
}

#[test]
fn spawn_send_status_and_kill() {
    let env = Env::new();
    let out = env.spawn("a1", "claude", &[], &[]);
    assert!(out.starts_with("work-dir/a1 %"), "{out}");
    let st = env.status("a1");
    assert_eq!(st["state"], "idle");
    assert_eq!(st["driver"], "claude");
    assert_eq!(st["tmux"]["session"], "work-dir");
    assert_eq!(st["project"], "work-dir");
    assert!(st["pid"].as_u64().is_some());
    assert!(
        env.ok(&["list"])
            .lines()
            .any(|l| l.starts_with("work-dir/a1 "))
    );
    assert_eq!(env.ok(&["list", "-q"]), "work-dir/a1\n");

    // Multi-line Japanese text arrives as one prompt.
    env.ok(&["send", "a1", "こんにちは\n二行目\tタブ"]);
    // The fake CLI logs after the hook returns: wait for the log.
    env.until("the prompt", || {
        env.prompts("a1") == ["こんにちは\n二行目\tタブ"]
    });
    assert_eq!(env.status("a1")["holds"].as_sequence().unwrap().len(), 0);
    assert!(!env.agent_dir("a1").join("input.yml").exists());
    env.ok(&["wait", "a1", "--timeout", "10"]);

    // Text from standard input and from a file.
    let f = env.tmp.path().join("req.txt");
    std::fs::write(&f, "from file\n").unwrap();
    env.ok(&["send", "a1", "-f", f.to_str().unwrap()]);
    env.until("the prompt from the file", || {
        env.prompts("a1").last().is_some_and(|p| p == "from file")
    });

    // The pane shows its state through pane options.
    let opt = env.tmux(&[
        "show-options",
        "-p",
        "-t",
        "=work-dir:=a1",
        "-v",
        "@tmac_state",
    ]);
    assert!(!String::from_utf8_lossy(&opt.stdout).trim().is_empty());

    env.ok(&["kill", "a1"]);
    assert!(!env.agent_dir("a1").exists());
    assert!(
        env.events("a1").iter().any(|e| e["event"] == "exit"),
        "graceful /exit"
    );
    let panes = env.tmux(&["list-panes", "-a", "-F", "#{@tmac_agent}"]);
    assert!(!String::from_utf8_lossy(&panes.stdout).contains("a1"));
    env.expect(&["status", "a1"], 4);
}

#[test]
fn human_input_holds_tmac_input() {
    let env = Env::new();
    env.spawn("h1", "claude", &[], &[]);
    // A person types directly in the pane.
    env.tmux(&["send-keys", "-t", "=work-dir:=h1", "-l", "typed by hand"]);
    env.tmux(&["send-keys", "-t", "=work-dir:=h1", "Enter"]);
    env.until("the human prompt", || {
        env.prompts("h1").contains(&"typed by hand".to_string())
    });
    env.until("the human hold", || env.status("h1")["holds"][0] == "human");
    env.expect(&["send", "h1", "--no-wait", "blocked"], 5);
    env.expect(&["kill", "h1"], 5);
    // The hold ends human_sec (2 s) after the agent became idle.
    env.ok(&["send", "h1", "--timeout", "15", "after the hold"]);
    env.until("the prompt after the hold", || {
        env.prompts("h1")
            .last()
            .is_some_and(|p| p == "after the hold")
    });
    assert!(!env.agent_dir("h1").join("holds/human.yml").exists());
    // Once it ran out, the hold does not return when the agent is idle again.
    env.ok(&["wait", "h1", "--timeout", "10"]);
    assert!(env.status("h1")["holds"].as_sequence().unwrap().is_empty());
    env.ok(&["kill", "h1"]);
}

#[test]
fn manual_hold_and_release() {
    let env = Env::new();
    env.spawn("m1", "claude", &[], &[]);
    env.expect(&["hold", "m1", "--key", "human"], 2);
    env.ok(&[
        "hold",
        "m1",
        "--key",
        "q-1",
        "--note",
        "waiting for a person",
    ]);
    assert_eq!(env.status("m1")["holds"][0], "q-1");
    env.expect(&["send", "m1", "--no-wait", "x"], 5);
    env.expect(&["kill", "m1"], 5);
    let out = env.tmac(&["release", "m1", "--key", "nope"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no hold"));
    env.ok(&["release", "m1", "--key", "q-1"]);
    env.ok(&["send", "m1", "--no-wait", "x"]);
    env.ok(&["hold", "m1"]);
    env.ok(&["kill", "m1", "--force"]);
    assert!(!env.agent_dir("m1").exists());
}

#[test]
fn wake_waits_for_a_stop_or_types_when_idle() {
    let env = Env::new();
    env.spawn("w1", "claude", &[], &[]);
    // Busy: wake returns after the next Stop, without typing.
    env.ok(&["send", "w1", "busy 2"]);
    assert_eq!(state(&env, "w1"), "busy");
    let start = Instant::now();
    env.ok(&["wake", "w1", "news while busy", "--timeout", "15"]);
    assert!(start.elapsed() >= Duration::from_millis(500));
    assert!(!env.prompts("w1").contains(&"news while busy".to_string()));
    // Idle: wake types the text.
    env.ok(&["wake", "w1", "news while idle", "--timeout", "15"]);
    env.until("the news", || {
        env.prompts("w1")
            .last()
            .is_some_and(|p| p == "news while idle")
    });
    env.ok(&["kill", "w1"]);
}

#[test]
fn waiting_state_blocks_input() {
    let env = Env::new();
    env.spawn("p1", "claude", &[], &[]);
    env.ok(&["send", "p1", "ask"]);
    env.ok(&["wait", "p1", "--state", "waiting", "--timeout", "10"]);
    env.expect(&["send", "p1", "--no-wait", "x"], 5);
    env.expect(&["wait", "p1", "--timeout", "1"], 3);
    env.tmux(&["send-keys", "-t", "=work-dir:=p1", "y"]);
    env.ok(&["wait", "p1", "--timeout", "10"]);
    env.ok(&["kill", "p1"]);
}

#[test]
fn relay_can_continue_the_stop_and_reject_input() {
    let env = Env::new();
    let marker = env.tmp.path().join("blocked-once");
    let stop = format!(
        "Stop=cat >/dev/null; if [ -e '{m}' ]; then echo plain; else touch '{m}'; echo '{{\"decision\":\"block\",\"reason\":\"go on\"}}'; fi",
        m = marker.display()
    );
    let prompt = "UserPromptSubmit=if grep -q deny; then echo denied >&2; exit 2; fi";
    env.spawn("r1", "claude", &["--relay", &stop, "--relay", prompt], &[]);
    env.ok(&["send", "r1", "first"]);
    env.ok(&["wait", "r1", "--timeout", "10"]);
    env.until("the continued stop", || {
        env.events("r1").iter().any(|e| e["event"] == "continued")
    });
    // The relay's stdout reaches the CLI unchanged.
    env.until("the relay output", || {
        env.events("r1")
            .iter()
            .any(|e| e["event"] == "Stop" && e["out"] == "plain")
    });
    let log = std::fs::read_to_string(env.agent_dir("r1").join("events.log")).unwrap();
    assert!(log.contains("Stop(continued)"), "{log}");

    env.expect(&["send", "r1", "please deny this"], 7);
    assert_eq!(state(&env, "r1"), "idle");
    assert!(env.status("r1")["holds"].as_sequence().unwrap().is_empty());
    env.ok(&["kill", "r1"]);
}

#[test]
fn relay_timeout_is_enforced() {
    let env = Env::with_config("relay:\n  timeout: 1\n");
    env.spawn("t1", "claude", &["--relay", "Stop=sleep 30"], &[]);
    let start = Instant::now();
    env.ok(&["send", "t1", "x"]);
    env.ok(&["wait", "t1", "--timeout", "15"]);
    assert!(start.elapsed() < Duration::from_secs(10));
    env.until("the Stop hook", || {
        env.events("t1").iter().any(|e| e["event"] == "Stop")
    });
    let stop = env
        .events("t1")
        .into_iter()
        .find(|e| e["event"] == "Stop")
        .unwrap();
    assert_eq!(stop["code"], 1);
    env.ok(&["kill", "t1"]);
}

#[test]
fn unconfirmed_input_is_held() {
    let env = Env::new();
    env.spawn("u1", "claude", &["-e", "FAKE_NO_PROMPT_HOOK=1"], &[]);
    env.expect(&["send", "u1", "lost"], 6);
    assert_eq!(env.status("u1")["holds"][0], "unconfirmed");
    let hold = std::fs::read_to_string(env.agent_dir("u1").join("holds/unconfirmed.yml")).unwrap();
    assert!(hold.contains("sha256:") && !hold.contains("lost"), "{hold}");
    env.expect(&["send", "u1", "--no-wait", "next"], 5);
    env.ok(&["release", "u1", "--key", "unconfirmed"]);
    env.expect(&["send", "u1", "next"], 6);
    env.ok(&["kill", "u1", "--force"]);
}

#[test]
fn exited_agents_and_stale_hooks() {
    let env = Env::new();
    env.spawn("e1", "claude", &[], &[]);
    let old = env.field("e1", "instance");
    env.ok(&["send", "e1", "crash"]);
    env.until("exited", || state(&env, "e1") == "exited");
    assert_eq!(env.field("e1", "exit_status"), "3");
    env.expect(&["send", "e1", "x"], 4);
    env.expect(&["wait", "e1"], 4);
    let fake = common::fake_as(&env, "claude");
    let out = env.tmac(&["spawn", "e1", "--", fake.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("tmac kill work-dir/e1"));
    env.ok(&["kill", "e1"]);

    env.spawn("e1", "claude", &[], &[]);
    let new = env.field("e1", "instance");
    assert_ne!(old, new);
    let before = std::fs::read_to_string(env.agent_dir("e1").join("state.yml")).unwrap();
    let mut hook = env.cmd(&[
        "hook",
        "--project",
        "work-dir",
        "--agent",
        "e1",
        "--instance",
        &old,
        "claude",
        "Stop",
    ]);
    let out = hook.output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("earlier work-dir/e1"));
    let after = std::fs::read_to_string(env.agent_dir("e1").join("state.yml")).unwrap();
    assert_eq!(before, after);
    env.ok(&["kill", "e1"]);
}

#[test]
fn codex_driver() {
    let env = Env::new();
    env.spawn("c1", "codex", &[], &["first prompt"]);
    assert_eq!(env.prompts("c1"), ["first prompt"]);
    let agent = std::fs::read_to_string(env.agent_dir("c1").join("agent.yml")).unwrap();
    assert!(
        agent.contains("hook codex Stop") && !agent.contains("--instance"),
        "{agent}"
    );
    env.ok(&["send", "c1", "second"]);
    // Esc fires Interrupt, which makes the agent idle again.
    env.ok(&["send", "c1", "busy 30"]);
    assert_eq!(state(&env, "c1"), "busy");
    env.tmux(&["send-keys", "-t", "=work-dir:=c1", "Escape"]);
    env.ok(&["wait", "c1", "--timeout", "10"]);
    env.until("the Interrupt hook", || {
        env.events("c1").iter().any(|e| e["event"] == "Interrupt")
    });
    env.ok(&["kill", "c1"]);
    assert!(
        env.events("c1").iter().any(|e| e["event"] == "exit"),
        "graceful /quit"
    );
}

#[test]
fn codex_without_a_first_prompt_times_out() {
    let env = Env::new();
    let fake = common::fake_as(&env, "codex");
    env.expect(
        &[
            "spawn",
            "c2",
            "--timeout",
            "2",
            "--",
            fake.to_str().unwrap(),
        ],
        3,
    );
    assert_eq!(state(&env, "c2"), "starting");
    env.ok(&["kill", "c2", "--force"]);
}

#[test]
fn foreign_hooks_are_refused() {
    let env = Env::new();
    std::fs::create_dir_all(env.work.join(".claude")).unwrap();
    std::fs::write(
        env.work.join(".claude/settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"lmm hook"}]}]}}"#,
    )
    .unwrap();
    let fake = common::fake_as(&env, "claude");
    let out = env.tmac(&["spawn", "f1", "--", fake.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains(".claude/settings.json: hooks.Stop"));
    assert!(!env.agent_dir("f1").exists());
    env.spawn("f1", "claude", &["--allow-foreign-hooks"], &[]);
    env.ok(&["kill", "f1"]);
}

#[test]
fn usage_errors() {
    let env = Env::new();
    env.expect(&["spawn", "Bad", "--", "claude"], 2);
    env.expect(&["spawn", "x", "--", "bash"], 2);
    env.expect(
        &[
            "spawn",
            "x",
            "--driver",
            "claude",
            "--relay",
            "Nope=true",
            "--",
            "claude",
        ],
        2,
    );
    env.expect(&["spawn", "x", "-e", "TMAC_AGENT=y", "--", "claude"], 2);
    env.expect(&["send", "nobody", "x"], 4);
    env.expect(&["send", "nobody", "a\x1bb"], 2);
    env.expect(&["status", "nobody"], 4);
    env.expect(&["wait", "nobody", "--state", "sleepy"], 2);
    // A hook never fails with 2, even on bad arguments.
    env.expect(&["hook"], 1);
    // Without an agent in the environment, a hook does nothing.
    env.expect(&["hook", "claude", "Stop"], 0);
}

#[test]
fn projects_keep_agents_apart() {
    let env = Env::new();
    // Two repositories run an agent named "coder"; one runs in a worktree.
    let a = env.git_repo("Repo-A");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&a)
        .args(["worktree", "add", "-q", ".worktree/feat", "-b", "feat"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let wt = a.join(".worktree/feat");
    let b = env.git_repo("repo.b");
    let out = env.spawn("coder", "claude", &["-C", wt.to_str().unwrap()], &[]);
    assert!(out.starts_with("repo-a/coder %"), "{out}");
    env.spawn("coder", "claude", &["-C", b.to_str().unwrap()], &[]);

    // Elsewhere, agents are named with their project.
    assert_eq!(
        env.field("repo-a/coder", "cwd"),
        wt.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(env.field("repo.b/coder", "tmux.session"), "repo-b");
    env.expect(&["status", "coder"], 4);
    assert_eq!(env.ok(&["list", "-q"]), "");
    assert_eq!(
        env.ok(&["list", "-a", "-q"]),
        "repo-a/coder\nrepo.b/coder\n"
    );

    // Inside a repository (or one of its worktrees) the short name is enough.
    let status_in = |dir: &std::path::Path| {
        let out = env
            .cmd(&["status", "coder", "--field", "project"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(status_in(&a), "repo-a");
    assert_eq!(status_in(&wt), "repo-a");
    assert_eq!(status_in(&b), "repo.b");
    let out = env.cmd(&["list", "-q"]).current_dir(&b).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "repo.b/coder\n");

    // -p and TMAC_PROJECT choose the project explicitly.
    env.ok(&["-p", "repo.b", "send", "coder", "via -p"]);
    let out = env
        .cmd(&["send", "coder", "via env"])
        .env("TMAC_PROJECT", "repo-a")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    env.expect(&["-p", "Bad", "status", "coder"], 2);

    env.ok(&["kill", "repo-a/coder"]);
    env.ok(&["-p", "repo.b", "kill", "coder"]);
    assert_eq!(env.ok(&["list", "-a", "-q"]), "");
}
