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

impl Env {
    pub fn new() -> Env {
        Env::with_config("")
    }

    /// `extra` is appended to the configuration file.
    pub fn with_config(extra: &str) -> Env {
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
                "tmux:\n  socket_name: {socket}\nspawn:\n  timeout: 20\nsend:\n  settle_sec: 0.3\n  confirm_timeout: 3\nhold:\n  human_sec: 2\nkill:\n  timeout: 5\ndrivers:\n  claude:\n    submit_delay_ms: 100\n  codex:\n    submit_delay_ms: 100\n{extra}"
            ),
        )
        .unwrap();
        Env {
            tmp,
            home,
            work,
            socket,
        }
    }

    pub fn log_path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(format!("{name}.log"))
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_tmac"));
        c.args(args)
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
