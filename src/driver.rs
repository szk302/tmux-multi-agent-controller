//! Drivers (§3.2): what differs between AI CLIs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::record::State;
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Claude,
    Codex,
}

/// State change before the relay runs (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Before {
    None,
    /// Move to `busy` (or `waiting`).
    To(State),
    /// `UserPromptSubmit`: match the input, detect human input, move to `busy`.
    Prompt,
    /// `Stop`: count the stop (`stop_seq`).
    Stop,
}

/// State change after the relay ran (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    None,
    To(State),
    /// `UserPromptSubmit`: confirm or reject the input.
    Prompt,
    /// `Stop`: `idle`, or `busy` if the relay kept the agent going.
    Stop,
}

/// Command line and files prepared for spawn.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub command: Vec<String>,
    /// Files to place in the record's `driver/` directory.
    pub files: Vec<(String, Vec<u8>)>,
    /// Settings passed by the caller that tmac merged (claude `--settings`).
    pub caller_settings: Option<Value>,
}

/// Inputs to [`Driver::prepare`].
pub struct PrepareCtx<'a> {
    pub exe: &'a Path,
    pub project: &'a str,
    pub name: &'a str,
    pub instance: &'a str,
    /// Final path of the record's `driver/` directory.
    pub driver_dir: &'a Path,
    pub cwd: &'a Path,
    pub relay_timeout: f64,
}

const CLAUDE_EVENTS: &[&str] = &[
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

const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "Interrupt",
    "SessionEnd",
];

/// Events whose hooks can stop the agent from stopping or reject input,
/// which the state tracking assumes only tmac handles (§5.6).
const BLOCKING_EVENTS: &[&str] = &["Stop", "UserPromptSubmit"];

fn str_field<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload.get(key).and_then(Value::as_str).unwrap_or("")
}

impl Driver {
    pub fn from_name(s: &str) -> Option<Driver> {
        match s {
            "claude" => Some(Driver::Claude),
            "codex" => Some(Driver::Codex),
            _ => None,
        }
    }

    /// Pick the driver from the file name of the command.
    pub fn from_command(program: &str) -> Option<Driver> {
        let base = Path::new(program)
            .file_name()?
            .to_string_lossy()
            .into_owned();
        Driver::from_name(&base)
    }

    pub fn name(self) -> &'static str {
        match self {
            Driver::Claude => "claude",
            Driver::Codex => "codex",
        }
    }

    /// Events tmac registers.
    pub fn events(self) -> &'static [&'static str] {
        match self {
            Driver::Claude => CLAUDE_EVENTS,
            Driver::Codex => CODEX_EVENTS,
        }
    }

    /// Default delay between paste and Enter.
    pub fn submit_delay_default_ms(self) -> u64 {
        match self {
            Driver::Claude => 200,
            Driver::Codex => 500,
        }
    }

    /// Text typed to end the CLI normally (§11).
    pub fn exit_text(self) -> &'static str {
        match self {
            Driver::Claude => "/exit",
            Driver::Codex => "/quit",
        }
    }

    /// Hook time limit for an event, in seconds.
    fn hook_timeout(self, event: &str, relay_timeout: f64) -> u64 {
        if self == Driver::Codex && matches!(event, "Interrupt" | "SessionEnd") {
            // Codex clamps these to 3 seconds.
            return 3;
        }
        (relay_timeout + 10.0).ceil() as u64
    }

    pub fn before(self, event: &str, payload: &Value) -> Before {
        match event {
            "UserPromptSubmit" => Before::Prompt,
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Before::To(State::Busy),
            "PermissionRequest" => Before::To(State::Waiting),
            "Notification" if self == Driver::Claude => {
                match str_field(payload, "notification_type") {
                    "permission_prompt" | "elicitation_dialog" => Before::To(State::Waiting),
                    _ => Before::None,
                }
            }
            "Stop" => Before::Stop,
            _ => Before::None,
        }
    }

    pub fn after(self, event: &str, payload: &Value) -> After {
        match event {
            "UserPromptSubmit" => After::Prompt,
            "Stop" => After::Stop,
            "SessionStart" => {
                let source = str_field(payload, "source");
                let idle = match self {
                    // Claude fires SessionStart when it is ready for input.
                    Driver::Claude => matches!(source, "startup" | "resume" | "clear"),
                    // Codex fires it with the first prompt of a session; the
                    // prompt itself follows.
                    Driver::Codex => matches!(source, "resume" | "clear"),
                };
                if idle {
                    After::To(State::Idle)
                } else {
                    After::None
                }
            }
            "Notification" if self == Driver::Claude => {
                if str_field(payload, "notification_type") == "idle_prompt" {
                    After::To(State::Idle)
                } else {
                    After::None
                }
            }
            // Esc in Codex fires Interrupt instead of Stop.
            "Interrupt" if self == Driver::Codex => After::To(State::Idle),
            "SessionEnd" => match str_field(payload, "reason") {
                "clear" | "resume" => After::None,
                _ => After::To(State::Exited),
            },
            _ => After::None,
        }
    }

    /// Whether a submitted prompt was made by the CLI itself rather than
    /// typed: Claude Code hands notices of finished background tasks to the
    /// model as prompts (`<task-notification>`) and fires UserPromptSubmit
    /// for them (§9.3).
    pub fn generated_prompt(self, prompt: &str) -> bool {
        match self {
            Driver::Claude => prompt.trim_start().starts_with("<task-notification>"),
            Driver::Codex => false,
        }
    }

    /// Whether the relay stopped the stop or rejected the input (§6.3).
    pub fn relay_blocked(self, code: i32, stdout: &[u8]) -> bool {
        if code == 2 {
            return true;
        }
        code == 0
            && serde_json::from_slice::<Value>(stdout)
                .ok()
                .and_then(|v| {
                    v.get("decision")
                        .and_then(Value::as_str)
                        .map(|d| d == "block")
                })
                .unwrap_or(false)
    }

    /// Add tmac's hooks to the command line (§5.5).
    pub fn prepare(self, ctx: &PrepareCtx, command: &[String]) -> Result<Prepared> {
        match self {
            Driver::Claude => prepare_claude(ctx, command),
            Driver::Codex => prepare_codex(ctx, command),
        }
    }

    /// Places where hooks other than tmac's handle `Stop` or
    /// `UserPromptSubmit` (§5.6).
    pub fn foreign_hooks(self, cwd: &Path, prepared: &Prepared) -> Vec<String> {
        match self {
            Driver::Claude => claude_foreign_hooks(cwd, prepared.caller_settings.as_ref()),
            Driver::Codex => codex_foreign_hooks(cwd, &prepared.command),
        }
    }
}

/// Index of the CLI in the command: the first argument named like the
/// driver (so wrappers such as `env` or `mise exec --` keep working), or 0.
fn cli_index(command: &[String], driver: Driver) -> usize {
    command
        .iter()
        .position(|a| Path::new(a).file_name().is_some_and(|n| n == driver.name()))
        .unwrap_or(0)
}

fn hook_command(ctx: &PrepareCtx, driver: Driver, event: &str) -> String {
    let exe = util::sh_quote(&ctx.exe.to_string_lossy());
    match driver {
        Driver::Claude => format!(
            "{exe} hook --project {} --agent {} --instance {} claude {event}",
            ctx.project, ctx.name, ctx.instance
        ),
        // Codex trusts hooks by their definition, so keep the command the
        // same for every agent: the agent and instance come from the
        // environment that spawn set (§5.4, §6.1).
        Driver::Codex => format!("{exe} hook codex {event}"),
    }
}

/// Split off the caller's `--settings` value, if any.
fn take_settings_arg(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut rest = Vec::new();
    let mut value = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--settings" && i + 1 < args.len() {
            value = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--settings=") {
            value = Some(v.to_string());
            i += 1;
            continue;
        }
        rest.push(a.clone());
        i += 1;
    }
    (rest, value)
}

fn prepare_claude(ctx: &PrepareCtx, command: &[String]) -> Result<Prepared> {
    let at = cli_index(command, Driver::Claude);
    let (rest, caller) = take_settings_arg(&command[at + 1..]);
    let caller_settings = match caller {
        None => None,
        Some(v) if v.trim_start().starts_with('{') => Some(
            serde_json::from_str::<Value>(&v)
                .map_err(|e| Error::usage(format!("cannot parse --settings JSON: {e}")))?,
        ),
        Some(v) => {
            let path = ctx.cwd.join(&v);
            let text = std::fs::read_to_string(&path).map_err(|e| {
                Error::usage(format!("cannot read --settings {}: {e}", path.display()))
            })?;
            Some(
                serde_json::from_str::<Value>(&text)
                    .map_err(|e| Error::usage(format!("cannot parse {}: {e}", path.display())))?,
            )
        }
    };
    let mut settings = caller_settings.clone().unwrap_or_else(|| json!({}));
    if !settings.is_object() {
        return Err(Error::usage("--settings must be a JSON object"));
    }
    let obj = settings.as_object_mut().expect("object");
    if !obj.get("hooks").is_some_and(Value::is_object) {
        obj.insert("hooks".into(), json!({}));
    }
    let hooks = obj
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .expect("hooks");
    for &event in CLAUDE_EVENTS {
        let group = json!({
            "hooks": [{
                "type": "command",
                "command": hook_command(ctx, Driver::Claude, event),
                "timeout": Driver::Claude.hook_timeout(event, ctx.relay_timeout),
            }]
        });
        match hooks.get_mut(event).and_then(Value::as_array_mut) {
            Some(list) => list.push(group),
            None => {
                hooks.insert(event.into(), json!([group]));
            }
        }
    }
    let path = ctx.driver_dir.join("claude-settings.json");
    let data = serde_json::to_vec_pretty(&settings).map_err(|e| Error::runtime(e.to_string()))?;
    let mut cmd = command[..=at].to_vec();
    cmd.extend(["--settings".into(), path.to_string_lossy().into_owned()]);
    cmd.extend(rest);
    Ok(Prepared {
        command: cmd,
        files: vec![("claude-settings.json".into(), data)],
        caller_settings,
    })
}

fn prepare_codex(ctx: &PrepareCtx, command: &[String]) -> Result<Prepared> {
    let at = cli_index(command, Driver::Codex);
    let mut cmd = command[..=at].to_vec();
    for &event in CODEX_EVENTS {
        // A JSON string is also a valid TOML basic string.
        let command = serde_json::to_string(&hook_command(ctx, Driver::Codex, event))
            .map_err(|e| Error::runtime(e.to_string()))?;
        let timeout = Driver::Codex.hook_timeout(event, ctx.relay_timeout);
        cmd.push("-c".into());
        cmd.push(format!(
            "hooks.{event}=[{{hooks=[{{type=\"command\",command={command},timeout={timeout}}}]}}]"
        ));
    }
    cmd.extend(command[at + 1..].iter().cloned());
    Ok(Prepared {
        command: cmd,
        files: Vec::new(),
        caller_settings: None,
    })
}

/// Hook events that block in a parsed hooks object (`{"Stop": [...]}`).
fn blocking_in(hooks: Option<&Value>) -> Vec<&'static str> {
    let Some(hooks) = hooks.and_then(Value::as_object) else {
        return Vec::new();
    };
    BLOCKING_EVENTS
        .iter()
        .copied()
        .filter(|ev| {
            hooks
                .get(*ev)
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
        })
        .collect()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Git top level of a directory, if any.
fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn report(found: &mut Vec<String>, place: &str, events: &[&str]) {
    for ev in events {
        found.push(format!("{place}: hooks.{ev}"));
    }
}

fn claude_foreign_hooks(cwd: &Path, caller: Option<&Value>) -> Vec<String> {
    let mut found = Vec::new();
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".claude")));
    let mut files = Vec::new();
    if let Some(d) = &config_dir {
        files.push(d.join("settings.json"));
    }
    let mut project_dirs = vec![cwd.to_path_buf()];
    if let Some(top) = git_toplevel(cwd).filter(|t| t != cwd) {
        project_dirs.push(top);
    }
    for d in &project_dirs {
        files.push(d.join(".claude/settings.json"));
        files.push(d.join(".claude/settings.local.json"));
    }
    if cfg!(target_os = "macos") {
        files.push(PathBuf::from(
            "/Library/Application Support/ClaudeCode/managed-settings.json",
        ));
    } else {
        files.push(PathBuf::from("/etc/claude-code/managed-settings.json"));
    }
    let mut enabled: BTreeMap<String, bool> = BTreeMap::new();
    for f in &files {
        let Some(v) = read_json(f) else { continue };
        report(
            &mut found,
            &f.display().to_string(),
            &blocking_in(v.get("hooks")),
        );
        if let Some(p) = v.get("enabledPlugins").and_then(Value::as_object) {
            for (k, on) in p {
                enabled.insert(k.clone(), on.as_bool().unwrap_or(false));
            }
        }
    }
    if let Some(v) = caller {
        report(&mut found, "--settings", &blocking_in(v.get("hooks")));
    }
    // Hooks of enabled plugins: <installPath>/hooks/hooks.json.
    if let Some(d) = &config_dir
        && let Some(installed) = read_json(&d.join("plugins/installed_plugins.json"))
    {
        for (plugin, _) in enabled.iter().filter(|(_, on)| **on) {
            let entries = installed
                .get("plugins")
                .and_then(|p| p.get(plugin))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for e in entries {
                let Some(path) = e.get("installPath").and_then(Value::as_str) else {
                    continue;
                };
                let file = Path::new(path).join("hooks/hooks.json");
                if let Some(v) = read_json(&file) {
                    let hooks = v.get("hooks").or(Some(&v));
                    report(
                        &mut found,
                        &format!("plugin {plugin} ({})", file.display()),
                        &blocking_in(hooks),
                    );
                }
            }
        }
    }
    found
}

fn codex_toml_hooks(path: &Path) -> Vec<&'static str> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(hooks) = table.get("hooks").and_then(|h| h.as_table()) else {
        return Vec::new();
    };
    BLOCKING_EVENTS
        .iter()
        .copied()
        .filter(|ev| {
            hooks
                .get(*ev)
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty())
        })
        .collect()
}

fn codex_foreign_hooks(cwd: &Path, command: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    let codex_home = std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".codex")));
    let mut dirs: Vec<PathBuf> = Vec::new();
    dirs.push(PathBuf::from("/etc/codex"));
    if let Some(h) = codex_home {
        dirs.push(h);
    }
    // Project layers from the project root down to the working directory.
    let top = git_toplevel(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let mut layer = Vec::new();
    let mut d = cwd.to_path_buf();
    loop {
        layer.push(d.join(".codex"));
        if d == top || !d.pop() {
            break;
        }
    }
    layer.reverse();
    dirs.extend(layer);
    for d in &dirs {
        let hooks_json = d.join("hooks.json");
        if let Some(v) = read_json(&hooks_json) {
            report(
                &mut found,
                &hooks_json.display().to_string(),
                &blocking_in(v.get("hooks")),
            );
        }
        let config = d.join("config.toml");
        report(
            &mut found,
            &config.display().to_string(),
            &codex_toml_hooks(&config),
        );
    }
    // The caller's own -c hooks.* overrides (tmac's come first and are skipped).
    let mut i = 1;
    while i + 1 < command.len() {
        if command[i] == "-c" || command[i] == "--config" {
            let v = &command[i + 1];
            for ev in BLOCKING_EVENTS {
                if v.starts_with(&format!("hooks.{ev}=")) && !v.contains(" hook codex ") {
                    found.push(format!("command line: -c hooks.{ev}"));
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(dir: &'a Path) -> PrepareCtx<'a> {
        PrepareCtx {
            exe: Path::new("/opt/tmac bin/tmac"),
            project: "p1",
            name: "a1",
            instance: "00000000-0000-4000-8000-000000000000",
            driver_dir: dir,
            cwd: dir,
            relay_timeout: 20.0,
        }
    }

    #[test]
    fn driver_from_command() {
        assert_eq!(
            Driver::from_command("/usr/bin/claude"),
            Some(Driver::Claude)
        );
        assert_eq!(Driver::from_command("codex"), Some(Driver::Codex));
        assert_eq!(Driver::from_command("bash"), None);
    }

    #[test]
    fn claude_settings_are_merged() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(
            t.path().join("s.json"),
            r#"{"permissions":{"allow":["Bash"]},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        )
        .unwrap();
        let cmd: Vec<String> = ["claude", "--settings", "s.json", "-p", "hi"]
            .map(String::from)
            .to_vec();
        let p = Driver::Claude.prepare(&ctx(t.path()), &cmd).unwrap();
        assert_eq!(p.command[1], "--settings");
        assert!(p.command[2].ends_with("claude-settings.json"));
        assert_eq!(&p.command[3..], ["-p", "hi"]);
        let v: Value = serde_json::from_slice(&p.files[0].1).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash");
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[1]["hooks"][0]["command"],
            "'/opt/tmac bin/tmac' hook --project p1 --agent a1 --instance 00000000-0000-4000-8000-000000000000 claude Stop"
        );
        let found = Driver::Claude.foreign_hooks(t.path(), &p);
        assert!(
            found.iter().any(|f| f == "--settings: hooks.Stop"),
            "{found:?}"
        );
    }

    #[test]
    fn codex_hooks_are_constant() {
        let t = tempfile::tempdir().unwrap();
        let cmd: Vec<String> = ["codex", "hello"].map(String::from).to_vec();
        let p = Driver::Codex.prepare(&ctx(t.path()), &cmd).unwrap();
        assert_eq!(p.command.last().unwrap(), "hello");
        let stop = p
            .command
            .iter()
            .find(|a| a.starts_with("hooks.Stop="))
            .unwrap();
        assert_eq!(
            stop,
            "hooks.Stop=[{hooks=[{type=\"command\",command=\"'/opt/tmac bin/tmac' hook codex Stop\",timeout=30}]}]"
        );
        let toml_value: toml::Table = format!("v = {}", &stop["hooks.Stop=".len()..])
            .parse()
            .unwrap();
        assert!(toml_value["v"].as_array().is_some());
        assert!(
            p.command
                .iter()
                .any(|a| a.contains("Interrupt") && a.contains("timeout=3"))
        );
        assert!(
            Driver::Codex
                .foreign_hooks(t.path(), &p)
                .iter()
                .all(|f| !f.starts_with("command line"))
        );
    }

    #[test]
    fn hooks_go_after_the_cli_behind_wrappers() {
        let t = tempfile::tempdir().unwrap();
        let cmd: Vec<String> = ["agent-exec", "/usr/bin/claude", "-p", "x"]
            .map(String::from)
            .to_vec();
        let p = Driver::Claude.prepare(&ctx(t.path()), &cmd).unwrap();
        assert_eq!(
            &p.command[..3],
            ["agent-exec", "/usr/bin/claude", "--settings"]
        );
        assert_eq!(&p.command[4..], ["-p", "x"]);
        let cmd: Vec<String> = ["env", "A=1", "codex", "hi"].map(String::from).to_vec();
        let p = Driver::Codex.prepare(&ctx(t.path()), &cmd).unwrap();
        assert_eq!(&p.command[..4], ["env", "A=1", "codex", "-c"]);
        assert_eq!(p.command.last().unwrap(), "hi");
    }

    #[test]
    fn generated_prompts() {
        assert!(Driver::Claude.generated_prompt("<task-notification>\n<task-id>x</task-id>"));
        assert!(!Driver::Claude.generated_prompt("please look at <task-notification>"));
        assert!(!Driver::Codex.generated_prompt("<task-notification>"));
    }

    #[test]
    fn relay_block_detection() {
        let d = Driver::Claude;
        assert!(d.relay_blocked(2, b""));
        assert!(d.relay_blocked(0, br#"{"decision":"block","reason":"x"}"#));
        assert!(!d.relay_blocked(0, b"plain"));
        assert!(!d.relay_blocked(1, br#"{"decision":"block"}"#));
    }

    #[test]
    fn event_mapping() {
        let p = json!({"source": "startup"});
        assert_eq!(
            Driver::Claude.after("SessionStart", &p),
            After::To(State::Idle)
        );
        assert_eq!(Driver::Codex.after("SessionStart", &p), After::None);
        assert_eq!(
            Driver::Codex.after("Interrupt", &json!({})),
            After::To(State::Idle)
        );
        assert_eq!(
            Driver::Claude.before(
                "Notification",
                &json!({"notification_type": "permission_prompt"})
            ),
            Before::To(State::Waiting)
        );
        assert_eq!(
            Driver::Claude.after("SessionEnd", &json!({"reason": "clear"})),
            After::None
        );
        assert_eq!(
            Driver::Claude.after("SessionEnd", &json!({"reason": "logout"})),
            After::To(State::Exited)
        );
    }
}
