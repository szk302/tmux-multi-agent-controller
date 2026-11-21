//! Drivers (§3.2): what differs between AI CLIs, as YAML.
//!
//! A driver holds tables and values only (events and their meaning, keys of
//! the hook input, input and exit, where hooks are). The procedures (state
//! changes, input matching, holds, locks, relays) stay in the code. The
//! built-in drivers are embedded; `$TMAC_HOME/drivers/<NAME>.yaml` adds a
//! driver or replaces a built-in one as a whole.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use serde_yaml_ng::Value as Yaml;

use crate::error::{Error, Result};
use crate::home::Home;
use crate::record::State;
use crate::util;

/// Events whose hooks can stop the agent from stopping or reject input,
/// which the state tracking assumes only tmac handles (§5.6).
pub const BLOCKING_EVENTS: &[&str] = &["Stop", "UserPromptSubmit"];

const BUILTIN: &[(&str, &str)] = &[
    ("claude", include_str!("../drivers/claude.yaml")),
    ("codex", include_str!("../drivers/codex.yaml")),
];

/// State change before the relay runs (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Before {
    None,
    /// Move to `busy` (or `waiting`).
    To(State),
    /// `role: prompt`: match the input, detect human input, move to `busy`.
    Prompt,
    /// `role: stop`: count the stop (`stop_seq`).
    Stop,
}

/// State change after the relay ran (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    None,
    To(State),
    /// `role: prompt`: confirm or reject the input.
    Prompt,
    /// `role: stop`: `idle`, or `busy` if the relay kept the agent going.
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Prompt,
    Stop,
}

/// One rule of an event (§3.2).
#[derive(Debug, Clone, Default)]
struct Rule {
    role: Option<Role>,
    before: Option<State>,
    after: Option<State>,
    when: BTreeMap<String, Vec<String>>,
    unless: BTreeMap<String, Vec<String>>,
    new_session: bool,
}

/// Format of a settings file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Json,
    Toml,
}

/// Format of a caller's argument that holds hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgFormat {
    /// The value is a JSON object or the path to a JSON file.
    Json,
    /// The value is `KEY.EVENT=VALUE` with a TOML value.
    TomlAssign,
}

/// A place where hooks are looked for (§5.6).
#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    File {
        /// With `${VAR:-default}`, `~/`, `{project}`, `{project_layers}`.
        path: String,
        format: FileFormat,
        key: String,
        /// Only on this OS (`linux`, `macos`).
        os: Option<String>,
        /// A system-wide file (skipped in tests).
        system: bool,
    },
    Arg {
        names: Vec<String>,
        format: ArgFormat,
        key: String,
    },
    /// Hooks of enabled Claude Code plugins under `dir`.
    ClaudePlugins { dir: String },
}

#[derive(Debug, Clone)]
struct Rulesync {
    target: String,
    timeout: u64,
    timeouts: BTreeMap<String, u64>,
}

/// Where a driver came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Builtin,
    /// A user's file; whether it replaces a built-in driver.
    File(PathBuf, bool),
}

#[derive(Debug)]
struct Spec {
    name: String,
    source: Source,
    text: String,
    commands: Vec<String>,
    events: Vec<(String, Vec<Rule>)>,
    session_id_key: Option<String>,
    prompt_key: String,
    generated: Vec<String>,
    submit_delay_ms: u64,
    exit_text: Option<String>,
    blocked_json: Option<serde_json::Map<String, Value>>,
    default_timeout: Option<f64>,
    places: Vec<Place>,
    rulesync: Option<Rulesync>,
}

/// A driver: cheap to clone.
#[derive(Debug, Clone)]
pub struct Driver(Arc<Spec>);

// --- The file format -----------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    version: u32,
    #[serde(default)]
    commands: Vec<String>,
    events: serde_yaml_ng::Mapping,
    #[serde(default)]
    payload: RawPayload,
    #[serde(default)]
    prompt: RawPrompt,
    #[serde(default)]
    input: RawInput,
    #[serde(default)]
    relay: RawRelay,
    #[serde(default)]
    hooks: RawHooks,
    rulesync: Option<RawRulesync>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPayload {
    session_id: Option<String>,
    prompt: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPrompt {
    #[serde(default)]
    generated: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawInput {
    submit_delay_ms: Option<u64>,
    exit_text: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRelay {
    blocked_json: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawHooks {
    default_timeout: Option<f64>,
    #[serde(default)]
    places: Vec<RawPlace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlace {
    file: Option<String>,
    arg: Option<Yaml>,
    plugins: Option<String>,
    dir: Option<String>,
    format: Option<String>,
    key: Option<String>,
    os: Option<String>,
    #[serde(default)]
    system: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRulesync {
    target: String,
    timeout: Option<u64>,
    #[serde(default)]
    timeouts: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    role: Option<String>,
    before: Option<String>,
    after: Option<String>,
    #[serde(default)]
    when: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    unless: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    new_session: bool,
}

fn rule(raw: RawRule, event: &str) -> std::result::Result<Rule, String> {
    let role = match raw.role.as_deref() {
        None => None,
        Some("prompt") => Some(Role::Prompt),
        Some("stop") => Some(Role::Stop),
        Some(r) => return Err(format!("events.{event}: unknown role {r:?} (prompt, stop)")),
    };
    if role.is_some() && (raw.before.is_some() || raw.after.is_some()) {
        return Err(format!(
            "events.{event}: a rule with a role cannot have before or after"
        ));
    }
    let before = match raw.before.as_deref() {
        None => None,
        Some(s @ ("busy" | "waiting")) => State::parse(s),
        Some(s) => {
            return Err(format!(
                "events.{event}: before must be busy or waiting, not {s:?}"
            ));
        }
    };
    let after = match raw.after.as_deref() {
        None => None,
        Some(s @ ("idle" | "busy" | "waiting" | "exited")) => State::parse(s),
        Some(s) => {
            return Err(format!(
                "events.{event}: after must be idle, busy, waiting or exited, not {s:?}"
            ));
        }
    };
    Ok(Rule {
        role,
        before,
        after,
        when: raw.when,
        unless: raw.unless,
        new_session: raw.new_session,
    })
}

fn place(raw: RawPlace, i: usize) -> std::result::Result<Place, String> {
    let at = format!("hooks.places[{i}]");
    let kinds = [raw.file.is_some(), raw.arg.is_some(), raw.plugins.is_some()];
    if kinds.iter().filter(|k| **k).count() != 1 {
        return Err(format!("{at}: give one of file, arg and plugins"));
    }
    let key = raw.key.clone().unwrap_or_else(|| "hooks".into());
    if let Some(path) = raw.file {
        let format = match raw.format.as_deref() {
            None | Some("json") => FileFormat::Json,
            Some("toml") => FileFormat::Toml,
            Some(f) => {
                return Err(format!(
                    "{at}: format of a file must be json or toml, not {f:?}"
                ));
            }
        };
        if let Some(os) = &raw.os
            && !matches!(os.as_str(), "linux" | "macos")
        {
            return Err(format!("{at}: os must be linux or macos, not {os:?}"));
        }
        if raw.dir.is_some() {
            return Err(format!("{at}: dir is only for plugins"));
        }
        return Ok(Place::File {
            path,
            format,
            key,
            os: raw.os,
            system: raw.system,
        });
    }
    if raw.os.is_some() || raw.system {
        return Err(format!("{at}: os and system are only for files"));
    }
    if let Some(arg) = raw.arg {
        let names: Vec<String> = match arg {
            Yaml::String(s) => vec![s],
            Yaml::Sequence(items) => items
                .into_iter()
                .map(|v| match v {
                    Yaml::String(s) => Ok(s),
                    _ => Err(format!("{at}: arg must be a string or a list of strings")),
                })
                .collect::<std::result::Result<_, _>>()?,
            _ => return Err(format!("{at}: arg must be a string or a list of strings")),
        };
        if names.is_empty() {
            return Err(format!("{at}: arg is empty"));
        }
        let format = match raw.format.as_deref() {
            None | Some("json") => ArgFormat::Json,
            Some("toml-assign") => ArgFormat::TomlAssign,
            Some(f) => {
                return Err(format!(
                    "{at}: format of an arg must be json or toml-assign, not {f:?}"
                ));
            }
        };
        return Ok(Place::Arg { names, format, key });
    }
    match raw.plugins.as_deref() {
        Some("claude") => Ok(Place::ClaudePlugins {
            dir: raw
                .dir
                .ok_or_else(|| format!("{at}: plugins needs dir (the CLI's settings directory)"))?,
        }),
        Some(p) => Err(format!("{at}: unknown plugins {p:?} (claude)")),
        None => unreachable!("checked above"),
    }
}

fn parse_spec(name: &str, source: Source, text: &str) -> std::result::Result<Spec, String> {
    let raw: RawSpec = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    if raw.version != 1 {
        return Err(format!("version must be 1, not {}", raw.version));
    }
    let mut events = Vec::new();
    for (k, v) in raw.events {
        let event = k
            .as_str()
            .filter(|s| !s.is_empty() && !s.contains(char::is_whitespace))
            .ok_or("event names must be strings without spaces")?
            .to_string();
        let raws: Vec<RawRule> = match v {
            Yaml::Sequence(items) => items
                .into_iter()
                .map(serde_yaml_ng::from_value)
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| format!("events.{event}: {e}"))?,
            v @ Yaml::Mapping(_) => {
                vec![serde_yaml_ng::from_value(v).map_err(|e| format!("events.{event}: {e}"))?]
            }
            _ => return Err(format!("events.{event} must be a rule or a list of rules")),
        };
        if raws.is_empty() {
            return Err(format!("events.{event} has no rule"));
        }
        let rules = raws
            .into_iter()
            .map(|r| rule(r, &event))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        events.push((event, rules));
    }
    for role in [Role::Prompt, Role::Stop] {
        if !events
            .iter()
            .any(|(_, rules)| rules.iter().any(|r| r.role == Some(role)))
        {
            let r = if role == Role::Prompt {
                "prompt"
            } else {
                "stop"
            };
            return Err(format!("an event must have role: {r}"));
        }
    }
    let places = raw
        .hooks
        .places
        .into_iter()
        .enumerate()
        .map(|(i, p)| place(p, i))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if let Some(t) = raw.hooks.default_timeout
        && !(t.is_finite() && t > 0.0)
    {
        return Err("hooks.default_timeout must be a positive number".into());
    }
    let rulesync = raw.rulesync.map(|r| Rulesync {
        target: r.target,
        timeout: r.timeout.unwrap_or(30),
        timeouts: r.timeouts,
    });
    if let Some(r) = &rulesync {
        for ev in r.timeouts.keys() {
            if !events.iter().any(|(e, _)| e == ev) {
                return Err(format!("rulesync.timeouts.{ev}: no such event"));
            }
        }
    }
    Ok(Spec {
        name: name.to_string(),
        source,
        text: text.to_string(),
        commands: raw.commands,
        events,
        session_id_key: raw.payload.session_id,
        prompt_key: raw.payload.prompt.unwrap_or_else(|| "prompt".into()),
        generated: raw.prompt.generated,
        submit_delay_ms: raw.input.submit_delay_ms.unwrap_or(200),
        exit_text: raw.input.exit_text.filter(|t| !t.is_empty()),
        blocked_json: raw.relay.blocked_json,
        default_timeout: raw.hooks.default_timeout,
        places,
        rulesync,
    })
}

// --- Loading -------------------------------------------------------------

fn user_dir(home: &Home) -> PathBuf {
    home.root.join("drivers")
}

fn user_file(home: &Home, name: &str) -> PathBuf {
    user_dir(home).join(format!("{name}.yaml"))
}

fn builtin_text(name: &str) -> Option<&'static str> {
    BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

fn source_label(source: &Source) -> String {
    match source {
        Source::Builtin => "built-in".into(),
        Source::File(p, _) => p.display().to_string(),
    }
}

impl Driver {
    /// Parse a driver file.
    pub fn parse(name: &str, source: Source, text: &str) -> Result<Driver> {
        if !util::valid_name(name) {
            return Err(Error::usage(format!(
                "invalid driver name {name:?}: use 1-64 of a-z 0-9 . _ - starting with a-z or 0-9"
            )));
        }
        let label = source_label(&source);
        parse_spec(name, source, text)
            .map(|s| Driver(Arc::new(s)))
            .map_err(|e| Error::usage(format!("driver {name} ({label}): {e}")))
    }

    /// Parse a driver file given by its path; the name is the file stem.
    pub fn parse_file(path: &Path) -> Result<Driver> {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::usage(format!("cannot read {}: {e}", path.display())))?;
        let replaces = builtin_text(&name).is_some();
        Driver::parse(&name, Source::File(path.to_path_buf(), replaces), &text)
    }

    /// The driver of a name: the user's file, else the built-in one.
    pub fn load(home: &Home, name: &str) -> Result<Driver> {
        if !util::valid_name(name) {
            return Err(Error::usage(format!("unknown driver {name:?}")));
        }
        let file = user_file(home, name);
        if file.is_file() {
            return Driver::parse_file(&file);
        }
        match builtin_text(name) {
            Some(text) => Driver::parse(name, Source::Builtin, text),
            None => Err(Error::usage(format!(
                "unknown driver {name:?} (drivers: {})",
                Driver::names(home)?.join(", ")
            ))),
        }
    }

    /// Names of the drivers: built-in and the user's, sorted.
    pub fn names(home: &Home) -> Result<Vec<String>> {
        let mut names: Vec<String> = BUILTIN.iter().map(|(n, _)| n.to_string()).collect();
        match std::fs::read_dir(user_dir(home)) {
            Ok(entries) => {
                for e in entries {
                    let path = e?.path();
                    if path.extension().is_some_and(|x| x == "yaml")
                        && let Some(stem) =
                            path.file_stem().map(|s| s.to_string_lossy().into_owned())
                        && util::valid_name(&stem)
                        && !names.contains(&stem)
                    {
                        names.push(stem);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        names.sort();
        Ok(names)
    }

    /// Every driver.
    pub fn all(home: &Home) -> Result<Vec<Driver>> {
        Driver::names(home)?
            .iter()
            .map(|n| Driver::load(home, n))
            .collect()
    }

    /// The driver whose `commands` has the file name of the program.
    pub fn from_command(home: &Home, program: &str) -> Result<Option<Driver>> {
        let Some(base) = Path::new(program).file_name() else {
            return Ok(None);
        };
        let base = base.to_string_lossy();
        for d in Driver::all(home)? {
            if d.0.commands.iter().any(|c| *c == base) {
                return Ok(Some(d));
            }
        }
        Ok(None)
    }

    pub fn name(&self) -> &str {
        &self.0.name
    }

    pub fn source(&self) -> &Source {
        &self.0.source
    }

    /// The file as written.
    pub fn text(&self) -> &str {
        &self.0.text
    }

    /// File names of the CLI's command.
    pub fn commands(&self) -> &[String] {
        &self.0.commands
    }

    /// Events tmac's hooks are installed for, in the file's order.
    pub fn events(&self) -> Vec<&str> {
        self.0.events.iter().map(|(e, _)| e.as_str()).collect()
    }

    /// Delay between paste and Enter.
    pub fn submit_delay_ms(&self) -> u64 {
        self.0.submit_delay_ms
    }

    /// Text typed to end the CLI normally (§11); `None` to send SIGTERM.
    pub fn exit_text(&self) -> Option<&str> {
        self.0.exit_text.as_deref()
    }

    /// Key of the prompt text in the input of `role: prompt` events.
    pub fn prompt_key(&self) -> &str {
        &self.0.prompt_key
    }

    pub fn places(&self) -> &[Place] {
        &self.0.places
    }

    /// Name of the CLI in rulesync, if the driver can be installed with
    /// `tmac hooks rulesync` (§5.5).
    pub fn rulesync_target(&self) -> Option<&str> {
        self.0.rulesync.as_ref().map(|r| r.target.as_str())
    }

    /// Time limit written for tmac's hook of an event, in seconds (§5.5).
    pub fn hook_timeout(&self, event: &str) -> u64 {
        match &self.0.rulesync {
            Some(r) => r.timeouts.get(event).copied().unwrap_or(r.timeout),
            None => 30,
        }
    }

    /// Time limit the CLI uses for a hook without one, in seconds; `None`
    /// if unknown.
    pub fn default_hook_timeout(&self) -> Option<f64> {
        self.0.default_timeout
    }

    fn rule(&self, event: &str, payload: &Value) -> Option<&Rule> {
        let (_, rules) = self.0.events.iter().find(|(e, _)| e == event)?;
        rules.iter().find(|r| {
            r.when
                .iter()
                .all(|(k, vals)| vals.iter().any(|v| *v == field(payload, k)))
                && !r
                    .unless
                    .iter()
                    .any(|(k, vals)| vals.iter().any(|v| *v == field(payload, k)))
        })
    }

    pub fn before(&self, event: &str, payload: &Value) -> Before {
        match self.rule(event, payload) {
            Some(Rule {
                role: Some(Role::Prompt),
                ..
            }) => Before::Prompt,
            Some(Rule {
                role: Some(Role::Stop),
                ..
            }) => Before::Stop,
            Some(Rule {
                before: Some(s), ..
            }) => Before::To(*s),
            _ => Before::None,
        }
    }

    pub fn after(&self, event: &str, payload: &Value) -> After {
        match self.rule(event, payload) {
            Some(Rule {
                role: Some(Role::Prompt),
                ..
            }) => After::Prompt,
            Some(Rule {
                role: Some(Role::Stop),
                ..
            }) => After::Stop,
            Some(Rule { after: Some(s), .. }) => After::To(*s),
            _ => After::None,
        }
    }

    /// Session ID of the CLI in a hook input (§6.2).
    pub fn session_id(&self, payload: &Value) -> Option<String> {
        let key = self.0.session_id_key.as_deref()?;
        Some(field(payload, key)).filter(|s| !s.is_empty())
    }

    /// Whether the event starts another session of the same CLI (`/clear`,
    /// resuming another session), so that the agent's session ID may change
    /// (§6.2).
    pub fn starts_new_session(&self, event: &str, payload: &Value) -> bool {
        self.rule(event, payload).is_some_and(|r| r.new_session)
    }

    /// Whether a submitted prompt was made by the CLI itself rather than
    /// typed (for Claude Code, notices of finished background tasks, §9.3).
    pub fn generated_prompt(&self, prompt: &str) -> bool {
        let p = prompt.trim_start();
        self.0.generated.iter().any(|g| p.starts_with(g.as_str()))
    }

    /// Whether the relay stopped the stop or rejected the input (§6.3):
    /// exit code 2, or exit code 0 with the driver's `blocked_json`.
    pub fn relay_blocked(&self, code: i32, stdout: &[u8]) -> bool {
        if code == 2 {
            return true;
        }
        let Some(want) = &self.0.blocked_json else {
            return false;
        };
        code == 0
            && serde_json::from_slice::<Value>(stdout)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .is_some_and(|got| want.iter().all(|(k, v)| got.get(k) == Some(v)))
    }
}

/// A field of the hook input as text (empty if missing).
fn field(payload: &Value, key: &str) -> String {
    match payload.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub fn builtin(name: &str) -> Driver {
        Driver::parse(name, Source::Builtin, builtin_text(name).unwrap()).unwrap()
    }

    #[test]
    fn builtins_parse() {
        let claude = builtin("claude");
        assert_eq!(claude.events().len(), 9);
        assert_eq!(claude.events()[0], "SessionStart");
        assert_eq!(claude.exit_text(), Some("/exit"));
        assert_eq!(claude.submit_delay_ms(), 200);
        assert_eq!(claude.default_hook_timeout(), Some(60.0));
        let codex = builtin("codex");
        assert_eq!(codex.events().len(), 8);
        assert_eq!(codex.exit_text(), Some("/quit"));
        assert_eq!(codex.submit_delay_ms(), 500);
        assert_eq!(codex.default_hook_timeout(), None);
        assert!(codex.places().contains(&Place::Arg {
            names: vec!["-c".into(), "--config".into()],
            format: ArgFormat::TomlAssign,
            key: "hooks".into(),
        }));
    }

    #[test]
    fn hook_values() {
        let (claude, codex) = (builtin("claude"), builtin("codex"));
        assert_eq!(claude.hook_timeout("Stop"), 30);
        assert_eq!(codex.hook_timeout("Interrupt"), 3);
        assert_eq!(codex.rulesync_target(), Some("codexcli"));
        let p = json!({"session_id": "s1", "source": "clear"});
        assert_eq!(codex.session_id(&p).as_deref(), Some("s1"));
        assert_eq!(claude.session_id(&json!({"session_id": ""})), None);
        assert!(claude.starts_new_session("SessionStart", &p));
        assert!(!claude.starts_new_session("SessionStart", &json!({"source": "startup"})));
        assert!(!claude.starts_new_session("Stop", &p));
    }

    #[test]
    fn generated_prompts() {
        assert!(builtin("claude").generated_prompt("<task-notification>\n<task-id>x</task-id>"));
        assert!(!builtin("claude").generated_prompt("please look at <task-notification>"));
        assert!(!builtin("codex").generated_prompt("<task-notification>"));
    }

    #[test]
    fn relay_block_detection() {
        let d = builtin("claude");
        assert!(d.relay_blocked(2, b""));
        assert!(d.relay_blocked(0, br#"{"decision":"block","reason":"x"}"#));
        assert!(!d.relay_blocked(0, b"plain"));
        assert!(!d.relay_blocked(1, br#"{"decision":"block"}"#));
    }

    #[test]
    fn event_mapping() {
        let (claude, codex) = (builtin("claude"), builtin("codex"));
        let p = json!({"source": "startup"});
        assert_eq!(claude.after("SessionStart", &p), After::To(State::Idle));
        assert_eq!(codex.after("SessionStart", &p), After::None);
        assert_eq!(codex.after("Interrupt", &json!({})), After::To(State::Idle));
        assert_eq!(
            claude.before(
                "Notification",
                &json!({"notification_type": "permission_prompt"})
            ),
            Before::To(State::Waiting)
        );
        assert_eq!(
            claude.after("Notification", &json!({"notification_type": "idle_prompt"})),
            After::To(State::Idle)
        );
        assert_eq!(
            claude.after("SessionEnd", &json!({"reason": "clear"})),
            After::None
        );
        assert_eq!(
            claude.after("SessionEnd", &json!({"reason": "logout"})),
            After::To(State::Exited)
        );
        assert_eq!(claude.before("Stop", &json!({})), Before::Stop);
        assert_eq!(claude.after("UserPromptSubmit", &json!({})), After::Prompt);
        assert_eq!(claude.before("Nope", &json!({})), Before::None);
    }

    #[test]
    fn invalid_files_are_reported() {
        let bad = |text: &str| match Driver::parse("x", Source::Builtin, text) {
            Ok(_) => panic!("accepted: {text}"),
            Err(e) => e.to_string(),
        };
        let ok = "version: 1\nevents:\n  P: {role: prompt}\n  S: {role: stop}\n";
        assert!(Driver::parse("x", Source::Builtin, ok).is_ok());
        assert!(bad("version: 2\nevents: {}\n").contains("version must be 1"));
        assert!(bad("version: 1\nevents:\n  P: {role: prompt}\n").contains("role: stop"));
        assert!(bad(&format!("{ok}typo: 1\n")).contains("typo"));
        assert!(
            bad("version: 1\nevents:\n  P: {role: prompt}\n  S: {role: stop, after: idle}\n")
                .contains("cannot have before or after")
        );
        assert!(
            bad(
                "version: 1\nevents:\n  P: {role: prompt}\n  S: {role: stop}\n  T: {before: idle}\n"
            )
            .contains("before must be busy or waiting")
        );
        assert!(
            bad(&format!(
                "{ok}hooks:\n  places:\n    - {{file: a, arg: b}}\n"
            ))
            .contains("give one of")
        );
        assert!(
            bad(&format!(
                "{ok}rulesync:\n  target: t\n  timeouts: {{Nope: 3}}\n"
            ))
            .contains("no such event")
        );
        assert!(Driver::parse("Bad", Source::Builtin, ok).is_err());
    }
}
