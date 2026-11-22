//! End-to-end tests with a real tmux server and a fake CLI.

mod common;

use std::time::{Duration, Instant};

use common::{Env, PROJECT};

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
fn prompts_made_by_the_cli_are_not_human_input() {
    let env = Env::new();
    env.spawn("g1", "claude", &[], &[]);
    env.ok(&["send", "g1", "selfnote"]);
    env.ok(&["wait", "g1", "--timeout", "10"]);
    env.until("the notice prompt", || {
        env.events("g1")
            .iter()
            .filter(|e| e["event"] == "UserPromptSubmit")
            .count()
            == 2
    });
    assert!(env.status("g1")["holds"].as_sequence().unwrap().is_empty());
    env.ok(&["kill", "g1"]);
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
    // The driver adds its first prompt: Codex fires no hook until it gets one.
    env.spawn("c1", "codex", &[], &[]);
    let first = env.prompts("c1");
    assert_eq!(first.len(), 1);
    assert!(first[0].starts_with("Do nothing now"), "{first:?}");
    // tmac runs the command as given (its hooks are in the user's
    // settings), plus the first prompt.
    let agent: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        &std::fs::read_to_string(env.agent_dir("c1").join("agent.yml")).unwrap(),
    )
    .unwrap();
    let command: Vec<&str> = agent["command"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(command.len(), 2, "{command:?}");
    assert_eq!(command[1], first[0]);
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

    // The caller can give its own first prompt, or none.
    env.spawn("c3", "codex", &["--first-prompt", "my own"], &[]);
    assert_eq!(env.prompts("c3"), ["my own"]);
    env.ok(&["kill", "c3"]);
    env.spawn("c4", "codex", &["--no-first-prompt"], &["in the command"]);
    assert_eq!(env.prompts("c4"), ["in the command"]);
    env.ok(&["kill", "c4"]);
}

#[test]
fn codex_without_a_first_prompt_times_out() {
    let env = Env::new();
    let fake = common::fake_as(&env, "codex");
    let out = env.tmac(&[
        "spawn",
        "c2",
        "--no-first-prompt",
        "--timeout",
        "2",
        "--",
        fake.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(3));
    // The driver's hint is shown.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Codex needs an instruction"),
        "{out:?}"
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
    // The configuration can accept known places that never block.
    let accepting = Env::with_config("spawn:\n  allow_foreign_hooks:\n    - /\n");
    std::fs::create_dir_all(accepting.work.join(".claude")).unwrap();
    std::fs::copy(
        env.work.join(".claude/settings.json"),
        accepting.work.join(".claude/settings.json"),
    )
    .unwrap();
    accepting.spawn("f2", "claude", &[], &[]);
    accepting.ok(&["kill", "f2"]);
}

#[test]
fn hooks_without_a_command_are_refused_too() {
    // Claude Code runs prompt, agent, http and mcp_tool hooks on Stop and
    // UserPromptSubmit as well, and they can block.
    let env = Env::new();
    std::fs::create_dir_all(env.work.join(".claude")).unwrap();
    std::fs::write(
        env.work.join(".claude/settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"prompt","prompt":"Do the tests pass?"}]}]}}"#,
    )
    .unwrap();
    let fake = common::fake_as(&env, "claude");
    let out = env.tmac(&["spawn", "p1", "--", fake.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(".claude/settings.json: hooks.Stop (prompt)"),
        "{out:?}"
    );
    assert!(!env.agent_dir("p1").exists());
    let out = env.tmac(&["doctor", "claude"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("hooks.Stop (prompt)"),
        "{out:?}"
    );
    env.spawn("p1", "claude", &["--allow-foreign-hooks"], &[]);
    env.ok(&["kill", "p1"]);
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

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn spawn_needs_tmacs_hooks_once() {
    let env = Env::bare("");
    let fake = common::fake_as(&env, "claude");
    let out = env.tmac(&["spawn", "n1", "--", fake.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("not installed"), "{}", stderr(&out));
    assert!(stderr(&out).contains("tmac hooks rulesync"));
    assert!(!env.agent_dir("n1").exists());

    // Only some events: the missing ones are named.
    std::fs::write(
        env.claude_dir().join("settings.json"),
        common::hooks_json("claude", &["Stop"], 30).to_string(),
    )
    .unwrap();
    let out = env.tmac(&["spawn", "n1", "--", fake.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("missing: SessionStart"),
        "{}",
        stderr(&out)
    );

    // Twice: in the user's settings and in the caller's --settings.
    env.install_hooks();
    let twice = common::hooks_json("claude", &["Stop"], 30).to_string();
    let out = env.tmac(&[
        "spawn",
        "n1",
        "--",
        fake.to_str().unwrap(),
        "--settings",
        &twice,
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("more than once"), "{}", stderr(&out));
    assert!(stderr(&out).contains("Stop:"), "{}", stderr(&out));
}

#[test]
fn spawn_needs_tmac_in_path() {
    let env = Env::new();
    let fake = common::fake_as(&env, "claude");
    // tmux is there (spawn asks it for the session's environment), tmac not.
    let only_tmux = env.tmp.path().join("only-tmux");
    std::fs::create_dir_all(&only_tmux).unwrap();
    let tmux = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join("tmux"))
        .find(|p| p.is_file())
        .unwrap();
    std::os::unix::fs::symlink(tmux, only_tmux.join("tmux")).unwrap();
    let out = env
        .cmd(&["spawn", "n2", "--", fake.to_str().unwrap()])
        .env("PATH", &only_tmux)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("not in PATH"), "{}", stderr(&out));
}

#[test]
fn spawn_needs_hook_time_limits_for_relays() {
    let env = Env::with_config("relay:\n  timeout: 25\n");
    let fake = common::fake_as(&env, "claude");
    let out = env.tmac(&[
        "spawn",
        "n3",
        "--relay",
        "Stop=true",
        "--",
        fake.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("Stop (30s)"), "{}", stderr(&out));
    // Events without a relay do not matter.
    env.spawn("n3", "claude", &[], &[]);
    env.ok(&["kill", "n3"]);
}

#[test]
fn doctor_checks_without_starting() {
    let env = Env::new();
    let out = env.ok(&["doctor"]);
    assert!(out.contains("claude: tmac's hooks: ok (9 events"), "{out}");
    assert!(out.contains("codex: tmac's hooks: ok (8 events"), "{out}");
    assert!(out.contains("tmac in PATH: "), "{out}");
    std::fs::remove_file(env.codex_dir().join("hooks.json")).unwrap();
    let out = env.tmac(&["doctor", "codex"]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("codex: tmac's hooks: missing"), "{text}");
    assert!(!text.contains("claude:"), "{text}");
    env.expect(&["doctor", "nope"], 2);
}

#[test]
fn hooks_rulesync_writes_the_sources() {
    let env = Env::new();
    let dir = env.tmp.path().join("rs");
    let mine = env.tmp.path().join("mine.jsonc");
    std::fs::write(
        &mine,
        "// my hooks\n{\n  \"hooks\": {\"sessionStart\": [{\"command\": \"echo hi\"}]},\n}\n",
    )
    .unwrap();
    let out = env.ok(&[
        "hooks",
        "rulesync",
        dir.to_str().unwrap(),
        "--merge",
        mine.to_str().unwrap(),
    ]);
    assert_eq!(out.lines().count(), 2, "{out}");
    let read = |name: &str| -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(name)).unwrap();
        let body: String = text
            .lines()
            .filter(|l| !l.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        serde_json::from_str(&body).unwrap()
    };
    let hooks = read("hooks.jsonc");
    assert_eq!(hooks["hooks"]["sessionStart"][0]["command"], "echo hi");
    assert_eq!(
        hooks["codexcli"]["hooks"]["Stop"][0]["command"],
        "command -v tmac >/dev/null 2>&1 || exit 0; exec tmac hook codex Stop"
    );
    let config = read("rulesync.jsonc");
    assert_eq!(config["features"], serde_json::json!(["hooks"]));
}

#[test]
fn hooks_of_nested_clis_are_ignored() {
    let env = Env::new();
    env.spawn("x1", "claude", &[], &[]);
    let sid = env.field("x1", "session_id");
    assert!(!sid.is_empty());
    env.ok(&["send", "x1", "nested"]);
    env.ok(&["wait", "x1", "--timeout", "10"]);
    env.until("the nested hooks", || {
        env.events("x1").iter().any(|e| e["event"] == "nested")
    });
    let log = std::fs::read_to_string(env.agent_dir("x1").join("events.log")).unwrap();
    assert!(
        log.contains("SessionStart ignored (session nested)"),
        "{log}"
    );
    assert!(log.contains("Stop ignored (session nested)"), "{log}");
    assert!(log.contains("Stop ignored (a codex hook)"), "{log}");
    // Only the agent's own Stop counted.
    assert_eq!(log.matches(" Stop busy->idle").count(), 1, "{log}");
    assert_eq!(env.field("x1", "session_id"), sid);
    env.ok(&["kill", "x1"]);
}

#[test]
fn session_changes_with_clear() {
    for driver in ["claude", "codex"] {
        let env = Env::new();
        env.spawn("s1", driver, &[], &[]);
        let before = env.field("s1", "session_id");
        // A person clears the session in the pane (no UserPromptSubmit).
        env.tmux(&["send-keys", "-t", "=work-dir:=s1", "-l", "/clear"]);
        env.tmux(&["send-keys", "-t", "=work-dir:=s1", "Enter"]);
        env.until("the clear", || {
            env.events("s1").iter().any(|e| e["event"] == "cleared")
        });
        // tmac follows the new session, so its input is confirmed.
        env.ok(&["send", "s1", "after clear"]);
        env.ok(&["wait", "s1", "--timeout", "10"]);
        let after = env.field("s1", "session_id");
        assert_ne!(before, after, "{driver}");
        // codex ends every session it had when it quits; only the current
        // one ends the agent.
        env.ok(&["kill", "s1"]);
        assert!(env.events("s1").iter().any(|e| e["event"] == "exit"));
    }
}

#[test]
fn driver_files_are_listed_shown_and_checked() {
    let env = Env::new();
    let list = env.ok(&["driver", "list"]);
    assert!(list.contains("claude\t"), "{list}");
    assert!(list.contains("(replaces the built-in one)"), "{list}");
    let shown = env.ok(&["driver", "show", "codex"]);
    assert!(shown.contains("submit_delay_ms: 100"), "{shown}");
    assert!(env.ok(&["driver", "check"]).contains("claude: ok"));
    // A broken file is reported, and spawn refuses to use it.
    let bad = env.home.join("drivers/broken.yaml");
    std::fs::write(&bad, "version: 1\nevents:\n  P: {role: prompt}\n").unwrap();
    let out = env.tmac(&["driver", "check", "broken"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("an event must have role: stop"),
        "{out:?}"
    );
    env.expect(&["driver", "check", bad.to_str().unwrap()], 2);
    env.expect(&["driver", "show", "nope"], 2);
}

#[test]
fn a_new_cli_runs_with_a_users_driver() {
    // "mycli": a CLI the built-in drivers do not know, described in YAML.
    // (The fake CLI acts like Claude Code and, under this name, reads its
    // hooks from $CODEX_HOME/hooks.json.)
    let env = Env::new();
    let yaml = common::CLAUDE_YAML
        .replace("commands: [claude]", "commands: [mycli]")
        .replace(
            "  places:\n",
            "  places:\n    - {file: \"${CODEX_HOME:-~/.codex}/hooks.json\", format: json}\n",
        );
    std::fs::write(env.home.join("drivers/mycli.yaml"), yaml).unwrap();
    std::fs::write(
        env.codex_dir().join("hooks.json"),
        common::hooks_json("mycli", common::CLAUDE_EVENTS, 30).to_string(),
    )
    .unwrap();
    let out = env.spawn("m1", "mycli", &[], &[]);
    assert!(out.starts_with("work-dir/m1 %"), "{out}");
    assert_eq!(env.field("m1", "driver"), "mycli");
    env.ok(&["send", "m1", "hello"]);
    env.ok(&["wait", "m1", "--timeout", "10"]);
    assert_eq!(env.prompts("m1"), ["hello"]);
    env.ok(&["kill", "m1"]);
    assert!(env.events("m1").iter().any(|e| e["event"] == "exit"));
}

#[test]
fn session_variables_of_a_parent_cli_are_removed() {
    let env = Env::new();
    let fake = common::fake_as(&env, "claude");
    let log = format!("FAKE_LOG={}", env.log_path("v1").display());
    // As if run from inside a Claude Code session (the tmux server and the
    // agent get this environment).
    let out = env
        .cmd(&["spawn", "v1", "-e", &log, "--", fake.to_str().unwrap()])
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_SESSION_ID", "parent")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let start = env
        .events("v1")
        .into_iter()
        .find(|e| e["event"] == "start")
        .unwrap();
    let vars = start["env"].as_object().unwrap();
    assert!(!vars.contains_key("CLAUDECODE"), "{vars:?}");
    assert!(!vars.contains_key("CLAUDE_CODE_SESSION_ID"), "{vars:?}");
    // The user's settings and tmac's variables stay.
    assert!(vars.contains_key("CLAUDE_CONFIG_DIR"), "{vars:?}");
    assert_eq!(vars["TMAC_AGENT"], "v1");
    // The record keeps the command as the CLI got it.
    let agent = std::fs::read_to_string(env.agent_dir("v1").join("agent.yml")).unwrap();
    assert!(!agent.contains("exec-clean"), "{agent}");
    env.ok(&["kill", "v1"]);
}

#[test]
fn the_variables_of_the_agent_that_made_the_session_stay_its_own() {
    // The first spawn makes the session (new-session -e): its variables
    // must not reach the next agents.
    let env = Env::new();
    env.spawn("s1", "claude", &["-e", "CLAUDE_TEST_MAILBOX=first"], &[]);
    env.spawn("s2", "claude", &[], &[]);
    let start = |name: &str| {
        env.events(name)
            .into_iter()
            .find(|e| e["event"] == "start")
            .unwrap()
    };
    assert_eq!(start("s1")["env"]["CLAUDE_TEST_MAILBOX"], "first");
    let second = start("s2");
    assert!(
        second["env"].get("CLAUDE_TEST_MAILBOX").is_none(),
        "{second}"
    );
    assert_eq!(second["env"]["TMAC_AGENT"], "s2");
    let shown = env.tmux(&["show-environment", "-t", &format!("={PROJECT}")]);
    let shown = String::from_utf8_lossy(&shown.stdout);
    for k in [
        "CLAUDE_TEST_MAILBOX",
        "TMAC_AGENT",
        "TMAC_INSTANCE",
        "FAKE_LOG",
    ] {
        assert!(!shown.contains(&format!("{k}=")), "{k} in {shown}");
    }
    env.ok(&["kill", "s1"]);
    env.ok(&["kill", "s2"]);
}

#[test]
fn relays_get_the_working_directory_and_whether_the_stop_continues() {
    let env = Env::new();
    let seen = env.tmp.path().join("seen");
    let marker = env.tmp.path().join("once");
    let stop = format!(
        "Stop=cat >/dev/null; echo \"$TMAC_CWD $TMAC_STOP_ACTIVE\" >> '{s}'; \
         if [ ! -e '{m}' ]; then touch '{m}'; echo again >&2; exit 2; fi",
        s = seen.display(),
        m = marker.display()
    );
    env.spawn("r2", "claude", &["--relay", &stop], &[]);
    env.ok(&["send", "r2", "x"]);
    env.ok(&["wait", "r2", "--timeout", "10"]);
    env.until("two stops", || {
        std::fs::read_to_string(&seen)
            .unwrap_or_default()
            .lines()
            .count()
            == 2
    });
    let cwd = env.work.canonicalize().unwrap();
    let lines: Vec<String> = std::fs::read_to_string(&seen)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(
        lines,
        [
            format!("{} false", cwd.display()),
            format!("{} true", cwd.display())
        ]
    );
    env.ok(&["kill", "r2"]);
}
