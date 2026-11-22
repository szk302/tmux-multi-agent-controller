//! A person at a terminal (§9.4), tmux sessions made from the terminal and
//! attaching to agents (§5.8, §10.1), project names (§3.1).

mod common;

use std::io::Write;
use std::process::Command;

use common::{Env, PROJECT};

fn holds(env: &Env, name: &str) -> Vec<String> {
    env.status(name)["holds"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_person_at_a_terminal_holds_input() {
    let env = Env::new();
    env.spawn("p1", "claude", &[], &[]);
    // The person's terminal shows another window of the session.
    let shell = format!("={PROJECT}:=shell");
    env.tmux(&[
        "new-window",
        "-d",
        "-t",
        &format!("={PROJECT}:"),
        "-n",
        "shell",
        "sleep 600",
    ]);
    env.tmux(&["select-window", "-t", &shell]);
    let mut term = env.terminal(&[
        "tmux",
        "-L",
        &env.socket,
        "attach",
        "-t",
        &format!("={PROJECT}"),
    ]);
    env.until("the terminal", || env.clients() == ["shell"]);
    // Keys pressed while looking at another window do not hold the agent.
    writeln!(term.stdin.as_mut().unwrap(), "x").unwrap();
    assert!(holds(&env, "p1").is_empty());
    // The person turns to the agent (no key press): tmac waits until
    // presence_sec (3 s) has passed since the last key press.
    env.tmux(&["select-window", "-t", &env.target("p1")]);
    env.until("the terminal on the agent", || env.clients() == ["p1"]);
    assert_eq!(holds(&env, "p1"), ["presence"]);
    env.expect(&["send", "p1", "--no-wait", "while typing"], 5);
    env.expect(&["kill", "p1"], 5);
    env.expect(&["hold", "p1", "--key", "presence"], 2);
    env.ok(&["send", "p1", "--timeout", "15", "after a pause"]);
    env.until("the prompt", || env.prompts("p1") == ["after a pause"]);
    assert!(holds(&env, "p1").is_empty());
    // A key press holds it again (pressed in the other window, so that the
    // key does not reach the agent).
    env.tmux(&["select-window", "-t", &shell]);
    env.until("the terminal on the shell", || env.clients() == ["shell"]);
    writeln!(term.stdin.as_mut().unwrap(), "y").unwrap();
    env.tmux(&["select-window", "-t", &env.target("p1")]);
    env.until("the hold", || holds(&env, "p1") == ["presence"]);
    drop(term.stdin.take());
    let _ = term.wait();
    env.until("the terminal to close", || env.clients().is_empty());
    assert!(holds(&env, "p1").is_empty());
    env.ok(&["kill", "p1"]);
}

#[test]
fn presence_can_be_turned_off() {
    let env = Env::new();
    env.edit_config("presence_sec: 3", "presence_sec: 0");
    env.spawn("p2", "claude", &[], &[]);
    let mut term = env.terminal(&[
        "tmux",
        "-L",
        &env.socket,
        "attach",
        "-t",
        &format!("={PROJECT}"),
    ]);
    // Attaching counts as a key press (tmux sets client_activity).
    env.until("the terminal", || env.clients() == ["p2"]);
    assert!(holds(&env, "p2").is_empty());
    env.ok(&["send", "p2", "--no-wait", "typed at once"]);
    drop(term.stdin.take());
    let _ = term.wait();
    env.ok(&["kill", "p2"]);
}

#[test]
fn sessions_are_made_from_the_terminal() {
    let env = Env::new();
    assert_eq!(env.ok(&["session"]), format!("{PROJECT}\n"));
    // Without --create, spawn --no-create-session refuses to make one.
    let fake = common::fake_as(&env, "claude");
    let out = env.tmac(&[
        "spawn",
        "s1",
        "--no-create-session",
        "--",
        fake.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("tmac session --create"));
    env.expect(&["status", "s1"], 4);
    assert!(
        !env.tmux(&["has-session", "-t", &format!("={PROJECT}")])
            .status
            .success()
    );
    // The terminal makes it; then spawn uses it.
    assert_eq!(env.ok(&["session", "--create"]), format!("{PROJECT}\n"));
    assert!(
        env.tmux(&["has-session", "-t", &format!("={PROJECT}")])
            .status
            .success()
    );
    env.ok(&["session", "--create"]);
    env.spawn("s1", "claude", &["--no-create-session"], &[]);
    // -p and tmux.session decide the name.
    assert_eq!(env.ok(&["-p", "other.proj", "session"]), "other-proj\n");
    let named = Env::new();
    named.edit_config("tmux:\n", "tmux:\n  session: agents\n");
    assert_eq!(named.ok(&["session"]), "agents\n");
    env.ok(&["kill", "s1"]);
}

#[test]
fn sessions_get_the_terminals_environment_on_a_running_server() {
    // The server already runs, started with other values (another
    // terminal, or a process with few variables).
    let env = Env::new();
    let stale = env.tmp.path().join("claude-stale");
    std::fs::create_dir_all(&stale).unwrap();
    let started = Command::new("tmux")
        .args([
            "-L",
            &env.socket,
            "new-session",
            "-d",
            "-s",
            "other",
            "sleep 600",
        ])
        .env("CLAUDE_CONFIG_DIR", &stale)
        .env("CLAUDE_TEST_GONE", "1")
        .env_remove("TMUX")
        .status()
        .unwrap();
    assert!(started.success());
    // The shell of a new session's first window writes its environment.
    let dump = env.tmp.path().join("first-shell.env");
    let shell = env.tmp.path().join("dump-shell");
    std::fs::write(
        &shell,
        format!("#!/bin/sh\nenv > '{}'\nexec sleep 600\n", dump.display()),
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    env.tmux(&["set-option", "-g", "default-shell", shell.to_str().unwrap()]);
    // The terminal: tmac's hooks in its CLAUDE_CONFIG_DIR, no
    // CLAUDE_TEST_GONE, inside an agent's pane (TMAC_AGENT), a secret, and
    // a variable longer than a tmux command line takes.
    let secret = "s3cret-value-of-the-test";
    let huge = "y".repeat(20000);
    let (bin, log) = env.logging_tmux();
    let out = env
        .cmd(&["session", "--create"])
        .env("PATH", env.path_with(&bin))
        .env("TMAC_AGENT", "outer")
        .env("CLAUDE_TEST_SECRET", secret)
        .env("CLAUDE_TEST_HUGE", &huge)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    let shown = env.tmux(&[
        "show-environment",
        "-t",
        &format!("={PROJECT}"),
        "TMAC_AGENT",
    ]);
    assert!(!shown.status.success(), "{shown:?}");
    // The first window's shell was started again with the same environment.
    env.until("the first shell to start again", || {
        std::fs::read_to_string(&dump).is_ok_and(|d| d.contains("CLAUDE_TEST_SECRET="))
    });
    let first = std::fs::read_to_string(&dump).unwrap();
    assert!(
        first.contains(&format!(
            "CLAUDE_CONFIG_DIR={}\n",
            env.claude_dir().display()
        )),
        "{first}"
    );
    assert!(!first.contains("CLAUDE_TEST_GONE="), "{first}");
    assert!(!first.contains("TMAC_AGENT=outer"), "{first}");
    env.spawn("e1", "claude", &["--no-create-session"], &[]);
    let start = env
        .events("e1")
        .into_iter()
        .find(|e| e["event"] == "start")
        .unwrap();
    assert_eq!(
        start["env"]["CLAUDE_CONFIG_DIR"].as_str(),
        env.claude_dir().to_str(),
        "{start}"
    );
    assert!(start["env"].get("CLAUDE_TEST_GONE").is_none(), "{start}");
    assert_eq!(
        start["env"]["CLAUDE_TEST_HUGE"].as_str(),
        Some(huge.as_str())
    );
    assert_eq!(start["env"]["CLAUDE_TEST_SECRET"], secret);
    assert_eq!(start["env"]["TMAC_AGENT"], "e1");
    // No value went through tmux's arguments.
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(argv.contains("source-file"), "{argv}");
    assert!(!argv.contains(secret), "{argv}");
    assert!(!argv.contains("yyyyyyyyyy"));
    env.ok(&["kill", "e1"]);
}

#[test]
fn attach_shows_the_agents_window() {
    let env = Env::new();
    env.spawn("a1", "claude", &[], &[]);
    env.tmux(&[
        "new-window",
        "-t",
        &format!("={PROJECT}:"),
        "-n",
        "shell",
        "sleep 600",
    ]);
    let term = env.terminal(&[env!("CARGO_BIN_EXE_tmac"), "attach", "a1"]);
    env.until("the terminal on the agent", || env.clients() == ["a1"]);
    drop(term);
    // Inside another tmux server, it refuses to nest, and leaves the
    // session's window as it was.
    let current = || {
        let out = env.tmux(&[
            "list-windows",
            "-t",
            &format!("={PROJECT}"),
            "-F",
            "#{window_active} #{window_name}",
        ]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("1 ").map(str::to_string))
            .unwrap_or_default()
    };
    env.tmux(&["select-window", "-t", &format!("={PROJECT}:=shell")]);
    assert_eq!(current(), "shell");
    let out = env
        .cmd(&["attach", "a1"])
        .env("TMUX", "/elsewhere/default,1,0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("inside another tmux server"));
    assert_eq!(current(), "shell");
    env.expect(&["attach", "nobody"], 4);
    env.ok(&["kill", "a1"]);
}

#[test]
fn project_names() {
    let env = Env::new();
    assert_eq!(env.ok(&["project"]), format!("{PROJECT}\n"));
    let repo = env.git_repo("My Repo");
    let wt = env.tmp.path().join("wt");
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "add", "-q", "--detach"])
        .arg(&wt)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        env.ok(&["project", "-C", wt.to_str().unwrap()]),
        "my-repo\n"
    );
    // TMAC_PROJECT (inside an agent) and -p count only without -C.
    let with_env = |args: &[&str]| {
        let out = env
            .cmd(args)
            .env("TMAC_PROJECT", "from-env")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert_eq!(with_env(&["project"]), "from-env\n");
    assert_eq!(
        with_env(&["project", "-C", repo.to_str().unwrap()]),
        "my-repo\n"
    );
    assert_eq!(env.ok(&["-p", "given", "project"]), "given\n");
    env.expect(&["project", "-C", "/no/such/dir"], 2);
}

#[test]
fn hooks_are_looked_for_in_the_sessions_environment() {
    // tmac's hooks are only in the configuration directory the session
    // gives the agent, not in the one of the caller (like Dagu's steps,
    // which get few variables).
    let env = Env::bare("");
    let theirs = env.tmp.path().join("claude-of-the-terminal");
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(
        theirs.join("settings.json"),
        common::hooks_json("claude", common::CLAUDE_EVENTS, 30).to_string(),
    )
    .unwrap();
    let fake = common::fake_as(&env, "claude");
    // Without a session, spawn looks where its own environment says.
    env.expect(&["spawn", "h1", "--", fake.to_str().unwrap()], 2);
    let made = Command::new("tmux")
        .args([
            "-L",
            &env.socket,
            "new-session",
            "-d",
            "-s",
            PROJECT,
            "sleep 600",
        ])
        .env("CLAUDE_CONFIG_DIR", &theirs)
        .env_remove("TMUX")
        .status()
        .unwrap();
    assert!(made.success());
    env.spawn("h1", "claude", &["--no-create-session"], &[]);
    assert_eq!(env.field("h1", "state"), "idle");
    env.ok(&["kill", "h1"]);
}
