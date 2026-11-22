//! Test harness: a private tmux server, data directory and fake CLI.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

pub struct Env {
    pub tmp: tempfile::TempDir,
    pub home: PathBuf,
    pub work: PathBuf,
    pub socket: String,
}

/// The project of the tests' working directory ("work dir").
pub const PROJECT: &str = "work-dir";

pub fn fake_cli() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_cli.py")
}

/// The fake CLI under the name of a driver (`<tmp>/bin/claude`).
pub fn fake_as(env: &Env, driver: &str) -> PathBuf {
    let bin = env.tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let link = bin.join(driver);
    if !link.exists() {
        std::os::unix::fs::symlink(fake_cli(), &link).unwrap();
    }
    link
}

pub const CLAUDE_YAML: &str = include_str!("../../drivers/claude.yaml");
pub const CODEX_YAML: &str = include_str!("../../drivers/codex.yaml");

pub const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "PermissionRequest",
    "Stop",
    "SessionEnd",
];

pub const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "Interrupt",
    "SessionEnd",
];

/// A hooks object as rulesync writes it for the user (§5.5).
pub fn hooks_json(driver: &str, events: &[&str], timeout: u64) -> Value {
    let mut m = serde_json::Map::new();
    for ev in events {
        m.insert(
            ev.to_string(),
            serde_json::json!([{"hooks": [{
                "type": "command",
                "command": format!(
                    "command -v tmac >/dev/null 2>&1 || exit 0; exec tmac hook {driver} {ev}"
                ),
                "timeout": timeout,
            }]}]),
        );
    }
    serde_json::json!({ "hooks": m })
}

/// The directory of the tmac binary under test, put first in PATH.
pub fn tmac_dir() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_tmac"))
        .parent()
        .unwrap()
        .to_path_buf()
}

impl Env {
    pub fn new() -> Env {
        Env::with_config("")
    }

    /// `extra` is appended to the configuration file.
    pub fn with_config(extra: &str) -> Env {
        let env = Env::bare(extra);
        env.install_hooks();
        env
    }

    /// Install tmac's hooks in the CLIs' user settings, like rulesync does.
    pub fn install_hooks(&self) {
        std::fs::write(
            self.claude_dir().join("settings.json"),
            hooks_json("claude", CLAUDE_EVENTS, 30).to_string(),
        )
        .unwrap();
        std::fs::write(
            self.codex_dir().join("hooks.json"),
            hooks_json("codex", CODEX_EVENTS, 30).to_string(),
        )
        .unwrap();
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.tmp.path().join("claude")
    }

    pub fn codex_dir(&self) -> PathBuf {
        self.tmp.path().join("codex")
    }

    /// Without tmac's hooks.
    pub fn bare(extra: &str) -> Env {
        let tmp = tempfile::Builder::new()
            .prefix("tmac-test.")
            .tempdir()
            .unwrap();
        let home = tmp.path().join("home");
        let work = tmp.path().join("work dir");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(tmp.path().join("claude")).unwrap();
        std::fs::create_dir_all(tmp.path().join("codex")).unwrap();
        let socket = format!("tmac-test-{}", uuid::Uuid::new_v4().simple());
        std::fs::write(
            home.join("config.yml"),
            format!(
                "tmux:\n  socket_name: {socket}\nsend:\n  settle_sec: 0.3\n  confirm_timeout: 3\nhold:\n  human_sec: 2\n  presence_sec: 3\nkill:\n  timeout: 5\n{extra}"
            ),
        )
        .unwrap();
        // The built-in drivers with a shorter paste delay, as the user's
        // driver files (they replace the built-in ones as a whole).
        let drivers = home.join("drivers");
        std::fs::create_dir_all(&drivers).unwrap();
        for (name, text, ms) in [("claude", CLAUDE_YAML, "200"), ("codex", CODEX_YAML, "500")] {
            let fast = text.replace(&format!("submit_delay_ms: {ms}"), "submit_delay_ms: 100");
            assert_ne!(fast, text);
            std::fs::write(drivers.join(format!("{name}.yaml")), fast).unwrap();
        }
        Env {
            tmp,
            home,
            work,
            socket,
        }
    }

    /// Replace text in the configuration file.
    pub fn edit_config(&self, from: &str, to: &str) {
        let path = self.home.join("config.yml");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{from:?} not in {text:?}");
        std::fs::write(path, text.replacen(from, to, 1)).unwrap();
    }

    pub fn log_path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(format!("{name}.log"))
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_tmac"));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![tmac_dir()];
        dirs.extend(std::env::split_paths(&path));
        c.args(args)
            .env("PATH", std::env::join_paths(dirs).unwrap())
            .env("TMAC_HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", self.tmp.path().join("claude"))
            .env("CODEX_HOME", self.tmp.path().join("codex"))
            .env_remove("TMUX")
            .env_remove("TMAC_AGENT")
            .env_remove("TMAC_PROJECT")
            .env_remove("TMAC_INSTANCE")
            .env_remove("TMAC_CONFIG")
            .current_dir(&self.work)
            .stdin(Stdio::null());
        c
    }

    pub fn tmac(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// Run and require exit code `code`; returns stdout.
    pub fn expect(&self, args: &[&str], code: i32) -> String {
        let out = self.tmac(args);
        assert_eq!(
            out.status.code(),
            Some(code),
            "tmac {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn ok(&self, args: &[&str]) -> String {
        self.expect(args, 0)
    }

    /// Spawn the fake CLI as `driver`; `extra` go before `--`.
    pub fn spawn(&self, name: &str, driver: &str, extra: &[&str], cli_args: &[&str]) -> String {
        let log = format!("FAKE_LOG={}", self.log_path(name).display());
        let fake = fake_as(self, driver);
        let mut args = vec!["spawn", name, "-e", &log];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--", fake.to_str().unwrap()]);
        args.extend_from_slice(cli_args);
        self.ok(&args)
    }

    /// A person's terminal running `argv` (see tests/fake_term.py): write
    /// lines to its stdin to press keys, drop the child to close it.
    pub fn terminal(&self, argv: &[&str]) -> std::process::Child {
        let c = self.cmd(&[]);
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_term.py");
        let mut all = vec![fake.to_str().unwrap()];
        all.extend_from_slice(argv);
        let mut t = Command::new(all[0]);
        t.args(&all[1..]);
        for (k, v) in c.get_envs() {
            match v {
                Some(v) => t.env(k, v),
                None => t.env_remove(k),
            };
        }
        t.current_dir(&self.work)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        t.spawn().unwrap()
    }

    /// `window_name` of every terminal (tmux client) of the test server.
    pub fn clients(&self) -> Vec<String> {
        let out = self.tmux(&["list-clients", "-F", "#{window_name}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(args)
            .output()
            .unwrap()
    }

    pub fn status(&self, name: &str) -> serde_yaml_ng::Value {
        serde_yaml_ng::from_str(&self.ok(&["status", name])).unwrap()
    }

    pub fn field(&self, name: &str, field: &str) -> String {
        self.ok(&["status", name, "--field", field])
            .trim()
            .to_string()
    }

    /// Events written by the fake CLI.
    pub fn events(&self, name: &str) -> Vec<Value> {
        std::fs::read_to_string(self.log_path(name))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn prompts(&self, name: &str) -> Vec<String> {
        self.events(name)
            .iter()
            .filter(|e| e["event"] == "prompt")
            .map(|e| e["prompt"].as_str().unwrap().to_string())
            .collect()
    }

    pub fn agent_dir(&self, name: &str) -> PathBuf {
        self.home.join("agents").join(PROJECT).join(name)
    }

    /// tmux target of an agent's window (the session is the project).
    pub fn target(&self, name: &str) -> String {
        format!("={PROJECT}:={name}")
    }

    /// A git repository with one commit under the temporary directory.
    pub fn git_repo(&self, dir: &str) -> PathBuf {
        let repo = self.tmp.path().join(dir);
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i",
            ],
        ] {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        repo
    }

    /// A directory with a `tmux` that records its arguments (one line per
    /// call) in the returned log, then runs the real tmux.
    pub fn logging_tmux(&self) -> (PathBuf, PathBuf) {
        let real = Command::new("sh")
            .args(["-c", "command -v tmux"])
            .output()
            .unwrap();
        let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
        let bin = self.tmp.path().join("logging-bin");
        let log = self.tmp.path().join("tmux-argv.log");
        std::fs::create_dir_all(&bin).unwrap();
        let wrapper = bin.join("tmux");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{real}' \"$@\"\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        (bin, log)
    }

    /// PATH with `first` before the directory of tmac and the test's PATH.
    pub fn path_with(&self, first: &Path) -> String {
        format!(
            "{}:{}:{}",
            first.display(),
            tmac_dir().display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// Wait until `f` holds (up to 20 s).
    pub fn until(&self, what: &str, f: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if f() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
        // tmux leaves the socket file behind.
        let dir = std::env::var_os("TMUX_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/tmp".into());
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let _ = std::fs::remove_file(dir.join(format!("tmux-{uid}")).join(&self.socket));
    }
}
