//! Hooks registered with the CLIs (§5.5, §5.6): where they are, which of
//! them are tmac's, and whether an agent can be started with them.
//!
//! tmac does not write the CLIs' settings. Its hooks are installed once for
//! the user (with rulesync, `tmac hooks rulesync`), and `spawn` and
//! `tmac doctor` read the settings to check them. Where to look comes from
//! the driver (`hooks.places`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::driver::{ArgFormat, BLOCKING_EVENTS, Driver, FileFormat, Place};
use crate::error::{Error, Result};

/// One hook found in a place.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Where it is, as reported to the user (a file path, `--settings`,
    /// `plugin NAME (PATH)`, `command line`).
    pub place: String,
    pub event: String,
    /// The hook's `type`: `command` (also when not given), or another kind
    /// the CLI runs itself, such as Claude Code's `prompt`, `agent`,
    /// `http` and `mcp_tool` (§5.6).
    pub kind: String,
    /// The command; empty for the other kinds.
    pub command: String,
    /// Seconds, if the hook sets a time limit.
    pub timeout: Option<f64>,
}

/// The guarded command tmac's hooks run (§5.5): it does nothing where tmac
/// is not installed.
pub fn hook_command(driver: &str, event: &str) -> String {
    format!("command -v tmac >/dev/null 2>&1 || exit 0; exec tmac hook {driver} {event}")
}

/// The driver and event of a hook command that runs `tmac hook`, if it is
/// one: `... tmac hook DRIVER EVENT`, also with a path to tmac and with
/// `--project`/`--agent`/`--instance` options.
pub fn tmac_hook_target(command: &str) -> Option<(String, String)> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let at = tokens.iter().rposition(|t| *t == "hook")?;
    let exe = tokens.get(at.checked_sub(1)?)?.trim_matches(['\'', '"']);
    if Path::new(exe).file_name()? != "tmac" {
        return None;
    }
    let mut rest = &tokens[at + 1..];
    while rest.len() > 2 && rest[0].starts_with("--") {
        rest = &rest[2..];
    }
    match rest {
        [driver, event] => Some(((*driver).to_string(), (*event).to_string())),
        _ => None,
    }
}

/// Values for expanding the places: environment variables, and whether to
/// read system-wide files.
#[derive(Debug, Clone)]
pub struct Lookup {
    /// `None`: the process environment.
    vars: Option<BTreeMap<String, String>>,
    pub system: bool,
}

impl Lookup {
    pub fn from_env() -> Lookup {
        Lookup {
            vars: None,
            system: true,
        }
    }

    /// These variables (the environment the agent will get, §5.6) and the
    /// system-wide files.
    pub fn from_map(vars: BTreeMap<String, String>) -> Lookup {
        Lookup {
            vars: Some(vars),
            system: true,
        }
    }

    /// Fixed variables and no system-wide files (for tests).
    pub fn with_vars(vars: &[(&str, &str)]) -> Lookup {
        Lookup {
            vars: Some(
                vars.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            system: false,
        }
    }

    fn var(&self, key: &str) -> Option<String> {
        let v = match &self.vars {
            Some(m) => m.get(key).cloned(),
            None => std::env::var(key).ok(),
        };
        v.filter(|v| !v.is_empty())
    }

    /// Expand `${VAR}`, `${VAR:-default}` and a leading `~/`. `None` if a
    /// variable without a default is not set.
    pub fn expand(&self, text: &str) -> Option<String> {
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find("${") {
            out.push_str(&rest[..at]);
            let end = rest[at..].find('}')? + at;
            let inner = &rest[at + 2..end];
            let (name, default) = match inner.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (inner, None),
            };
            match self.var(name) {
                Some(v) => out.push_str(&v),
                None => out.push_str(default?),
            }
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        if let Some(tail) = out.strip_prefix("~/") {
            return Some(format!("{}/{tail}", self.var("HOME")?));
        }
        Some(out)
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn read_toml(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let table = text.parse::<toml::Table>().ok()?;
    serde_json::to_value(table).ok()
}

/// Hooks in a hooks object (`{"Stop": [{"hooks": [{...}]}]}`): every
/// entry with a `command` or a `type`, so that hooks without a command
/// (Claude Code's `prompt`, `agent`, `http`, `mcp_tool`), which can block
/// `Stop` and `UserPromptSubmit` as well, are not missed.
fn collect(place: &str, hooks: Option<&Value>, out: &mut Vec<Found>) {
    let Some(hooks) = hooks.and_then(Value::as_object) else {
        return;
    };
    for (event, groups) in hooks {
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for g in groups {
            // A group holds hooks; accept a bare hook, too.
            let list: Vec<&Value> = match g.get("hooks").and_then(Value::as_array) {
                Some(l) => l.iter().collect(),
                None => vec![g],
            };
            for h in list {
                let kind = h.get("type").and_then(Value::as_str);
                let command = h.get("command").and_then(Value::as_str);
                if kind.is_none() && command.is_none() {
                    continue;
                }
                out.push(Found {
                    place: place.to_string(),
                    event: event.clone(),
                    kind: kind.unwrap_or("command").to_string(),
                    command: command.unwrap_or("").to_string(),
                    timeout: h.get("timeout").and_then(Value::as_f64),
                });
            }
        }
    }
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

/// Index of the CLI in the command: the first argument named like one of
/// the driver's commands (so wrappers such as `env` or `mise exec --` keep
/// working), or 0.
fn cli_index(command: &[String], driver: &Driver) -> usize {
    command
        .iter()
        .position(|a| {
            Path::new(a)
                .file_name()
                .is_some_and(|n| driver.commands().iter().any(|c| n == c.as_str()))
        })
        .unwrap_or(0)
}

/// Directories a file place covers: `{project}` is the working directory
/// and the git root; `{project_layers}` is each directory from the git root
/// down to the working directory.
fn expand_dirs(path: &str, cwd: &Path) -> Vec<PathBuf> {
    let top = git_toplevel(cwd);
    let sub =
        |dir: &Path, marker: &str| PathBuf::from(path.replacen(marker, &dir.to_string_lossy(), 1));
    if path.contains("{project}") {
        let mut dirs = vec![cwd.to_path_buf()];
        if let Some(t) = top.filter(|t| t != cwd) {
            dirs.push(t);
        }
        return dirs.iter().map(|d| sub(d, "{project}")).collect();
    }
    if path.contains("{project_layers}") {
        let top = top.unwrap_or_else(|| cwd.to_path_buf());
        let mut layers = Vec::new();
        let mut d = cwd.to_path_buf();
        loop {
            layers.push(d.clone());
            if d == top || !d.pop() {
                break;
            }
        }
        layers.reverse();
        return layers.iter().map(|d| sub(d, "{project_layers}")).collect();
    }
    vec![PathBuf::from(path)]
}

/// Values of an option in the arguments (`--name VALUE` or `--name=VALUE`).
fn arg_values<'a>(args: &'a [String], names: &[String]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if names.iter().any(|n| n == a) && i + 1 < args.len() {
            out.push(args[i + 1].as_str());
            i += 2;
            continue;
        }
        if let Some((k, v)) = a.split_once('=')
            && k.starts_with("--")
            && names.iter().any(|n| n == k)
        {
            out.push(v);
        }
        i += 1;
    }
    out
}

/// Every hook command the CLI would run for an agent in `cwd` started with
/// `command`, from the driver's places.
pub fn find(
    driver: &Driver,
    lookup: &Lookup,
    cwd: &Path,
    command: &[String],
) -> Result<Vec<Found>> {
    let args = command.get(cli_index(command, driver) + 1..).unwrap_or(&[]);
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let mut found = Vec::new();
    // `enabledPlugins` of the settings files read so far (Claude Code).
    let mut enabled: BTreeMap<String, bool> = BTreeMap::new();
    for place in driver.places() {
        match place {
            Place::File {
                path,
                format,
                key,
                os: only,
                system,
            } => {
                if (*system && !lookup.system) || only.as_deref().is_some_and(|o| o != os) {
                    continue;
                }
                let Some(path) = lookup.expand(path) else {
                    continue;
                };
                for file in expand_dirs(&path, cwd) {
                    let v = match format {
                        FileFormat::Json => read_json(&file),
                        FileFormat::Toml => read_toml(&file),
                    };
                    let Some(v) = v else { continue };
                    collect(&file.display().to_string(), v.get(key), &mut found);
                    if let Some(p) = v.get("enabledPlugins").and_then(Value::as_object) {
                        for (k, on) in p {
                            enabled.insert(k.clone(), on.as_bool().unwrap_or(false));
                        }
                    }
                }
            }
            Place::Arg { names, format, key } => {
                for value in arg_values(args, names) {
                    match format {
                        ArgFormat::Json => {
                            let parsed: Value = if value.trim_start().starts_with('{') {
                                serde_json::from_str(value).map_err(|e| {
                                    Error::usage(format!("cannot parse {} JSON: {e}", names[0]))
                                })?
                            } else {
                                let path = cwd.join(value);
                                let text = std::fs::read_to_string(&path).map_err(|e| {
                                    Error::usage(format!(
                                        "cannot read {} {}: {e}",
                                        names[0],
                                        path.display()
                                    ))
                                })?;
                                serde_json::from_str(&text).map_err(|e| {
                                    Error::usage(format!("cannot parse {}: {e}", path.display()))
                                })?
                            };
                            collect(&names[0], parsed.get(key), &mut found);
                        }
                        ArgFormat::TomlAssign => {
                            if let Some((k, v)) = value.split_once('=')
                                && let Some(event) = k
                                    .trim()
                                    .strip_prefix(key.as_str())
                                    .and_then(|r| r.strip_prefix('.'))
                                && let Ok(t) = format!("v = {v}").parse::<toml::Table>()
                                && let Ok(v) = serde_json::to_value(&t["v"])
                            {
                                let mut hooks = serde_json::Map::new();
                                hooks.insert(event.to_string(), v);
                                collect("command line", Some(&Value::Object(hooks)), &mut found);
                            }
                        }
                    }
                }
            }
            Place::ClaudePlugins { dir } => {
                let Some(dir) = lookup.expand(dir) else {
                    continue;
                };
                let Some(installed) =
                    read_json(&Path::new(&dir).join("plugins/installed_plugins.json"))
                else {
                    continue;
                };
                // Hooks of enabled plugins: <installPath>/hooks/hooks.json.
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
                            collect(
                                &format!("plugin {plugin} ({})", file.display()),
                                hooks,
                                &mut found,
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(found)
}

/// The result of checking the hooks for a driver (§5.6).
#[derive(Debug, Clone)]
pub struct Check {
    pub driver: Driver,
    /// tmac's hooks for each event of the driver.
    pub tmac: BTreeMap<String, Vec<Found>>,
    /// Places with other hooks on events that can block, as
    /// `PLACE: hooks.EVENT`, followed by ` (KIND)` for a hook that is not a
    /// command.
    pub foreign: Vec<String>,
    /// `tmac` in the PATH the agent gets.
    pub tmac_on_path: Option<PathBuf>,
}

impl Check {
    pub fn run(driver: &Driver, lookup: &Lookup, cwd: &Path, command: &[String]) -> Result<Check> {
        let found = find(driver, lookup, cwd, command)?;
        let mut tmac: BTreeMap<String, Vec<Found>> = driver
            .events()
            .iter()
            .map(|e| (e.to_string(), Vec::new()))
            .collect();
        let mut foreign = Vec::new();
        for f in found {
            let target = if f.kind == "command" {
                tmac_hook_target(&f.command)
            } else {
                None
            };
            match target {
                // tmac's hook for this driver, or for another one (it does
                // nothing in this CLI, §6.2).
                Some((d, e)) => {
                    if d == driver.name()
                        && e == f.event
                        && let Some(list) = tmac.get_mut(&f.event)
                    {
                        list.push(f);
                    }
                }
                None => {
                    if BLOCKING_EVENTS.contains(&f.event.as_str()) {
                        let mut line = format!("{}: hooks.{}", f.place, f.event);
                        if f.kind != "command" {
                            line.push_str(&format!(" ({})", f.kind));
                        }
                        if !foreign.contains(&line) {
                            foreign.push(line);
                        }
                    }
                }
            }
        }
        Ok(Check {
            driver: driver.clone(),
            tmac,
            foreign,
            tmac_on_path: which("tmac"),
        })
    }

    /// Events without tmac's hook, in the driver's order.
    pub fn missing(&self) -> Vec<String> {
        self.driver
            .events()
            .into_iter()
            .filter(|e| self.tmac.get(*e).is_none_or(Vec::is_empty))
            .map(str::to_string)
            .collect()
    }

    /// Events with tmac's hook more than once, and their places.
    pub fn duplicated(&self) -> Vec<(String, Vec<&str>)> {
        self.driver
            .events()
            .into_iter()
            .filter_map(|e| {
                let l = self.tmac.get(e)?;
                (l.len() > 1).then(|| (e.to_string(), l.iter().map(|f| f.place.as_str()).collect()))
            })
            .collect()
    }

    /// Relayed events whose hook time limit is shorter than the relay's
    /// plus 10 seconds (§6.3), with the limit.
    pub fn short_timeouts<'a>(
        &'a self,
        relayed: impl Iterator<Item = &'a str>,
        relay_timeout: f64,
    ) -> Vec<(&'a str, f64)> {
        let need = relay_timeout + 10.0;
        let mut out = Vec::new();
        for ev in relayed {
            let Some(list) = self.tmac.get(ev) else {
                continue;
            };
            for f in list {
                let t = f.timeout.or(self.driver.default_hook_timeout());
                if let Some(t) = t.filter(|t| *t < need) {
                    out.push((ev, t));
                }
            }
        }
        out
    }
}

/// An executable in PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// How to install tmac's hooks, for messages.
pub const INSTALL_HINT: &str = "install tmac's hooks for your user once: \
`tmac hooks rulesync ~/.tmac/rulesync`, then `rulesync generate --global -c rulesync.jsonc \
--input-roots .` in that directory (or add them by hand, see the README); \
check with `tmac doctor`";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Source;

    fn builtin(name: &str) -> Driver {
        let text = match name {
            "claude" => include_str!("../drivers/claude.yaml"),
            _ => include_str!("../drivers/codex.yaml"),
        };
        Driver::parse(name, Source::Builtin, text).unwrap()
    }

    fn lookup(root: &Path) -> Lookup {
        let (claude, codex) = (root.join("claude"), root.join("codex"));
        Lookup::with_vars(&[
            ("HOME", root.to_str().unwrap()),
            ("CLAUDE_CONFIG_DIR", claude.to_str().unwrap()),
            ("CODEX_HOME", codex.to_str().unwrap()),
        ])
    }

    #[test]
    fn tmac_hook_commands() {
        let t = |c: &str| tmac_hook_target(c).map(|(d, e)| format!("{d} {e}"));
        assert_eq!(
            t(&hook_command("claude", "Stop")).as_deref(),
            Some("claude Stop")
        );
        assert_eq!(
            t("'/opt/tmac bin/tmac' hook codex Stop").as_deref(),
            Some("codex Stop")
        );
        assert_eq!(
            t("/x/tmac hook --project p --agent a --instance i claude Stop").as_deref(),
            Some("claude Stop")
        );
        assert_eq!(t("lmm hook"), None);
        assert_eq!(t("tmacx hook claude Stop"), None);
        assert_eq!(t("tmac hook claude"), None);
    }

    #[test]
    fn expansion() {
        let l = Lookup::with_vars(&[("HOME", "/h"), ("A", "/a")]);
        assert_eq!(l.expand("${A:-~/x}/s.json").as_deref(), Some("/a/s.json"));
        assert_eq!(l.expand("${B:-~/x}/s.json").as_deref(), Some("/h/x/s.json"));
        assert_eq!(l.expand("${B}/s.json"), None);
        assert_eq!(l.expand("/etc/x").as_deref(), Some("/etc/x"));
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn hooks_in_json_toml_and_arguments() {
        let t = tempfile::tempdir().unwrap();
        let cwd = t.path().join("work");
        write(
            &cwd.join(".codex/hooks.json"),
            &format!(
                r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":"{}","timeout":30}}]}}]}}}}"#,
                hook_command("codex", "Stop")
            ),
        );
        write(
            &cwd.join(".codex/config.toml"),
            "[[hooks.UserPromptSubmit]]\nhooks = [{ type = \"command\", command = \"guard\" }]\n",
        );
        let cmd: Vec<String> = [
            "codex",
            "-c",
            "hooks.Stop=[{hooks=[{type=\"command\",command=\"other\"}]}]",
        ]
        .map(String::from)
        .to_vec();
        let found = find(&builtin("codex"), &lookup(t.path()), &cwd, &cmd).unwrap();
        let cmds: Vec<(&str, &str)> = found
            .iter()
            .map(|f| (f.event.as_str(), f.command.as_str()))
            .collect();
        assert!(cmds.contains(&("UserPromptSubmit", "guard")), "{found:?}");
        assert!(cmds.contains(&("Stop", "other")), "{found:?}");
        let stop = found
            .iter()
            .find(|f| f.place.ends_with("hooks.json"))
            .unwrap();
        assert_eq!(stop.timeout, Some(30.0));
    }

    #[test]
    fn check_counts_tmac_hooks_and_others() {
        let t = tempfile::tempdir().unwrap();
        let cwd = t.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let claude = builtin("claude");
        let hooks = |events: &[&str], extra: &str| {
            let mut m = serde_json::Map::new();
            for e in events {
                m.insert(
                    e.to_string(),
                    serde_json::json!([{"hooks": [{"type": "command",
                        "command": hook_command("claude", e), "timeout": 30}]}]),
                );
            }
            let mut v = serde_json::json!({"hooks": m});
            if !extra.is_empty() {
                v["hooks"]["Stop"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({"hooks": [{"type": "command", "command": extra}]}));
            }
            v.to_string()
        };
        write(
            &cwd.join(".claude/settings.json"),
            &hooks(&claude.events(), "lmm hook"),
        );
        let settings = hooks(&["Stop"], "");
        let cmd: Vec<String> = ["claude", "--settings", settings.as_str()]
            .map(String::from)
            .to_vec();
        let c = Check::run(&claude, &lookup(t.path()), &cwd, &cmd).unwrap();
        assert!(c.missing().is_empty());
        let dup = c.duplicated();
        assert_eq!(dup.len(), 1);
        assert_eq!(dup[0].0, "Stop");
        assert!(dup[0].1.contains(&"--settings"));
        assert_eq!(c.foreign.len(), 1);
        assert!(c.foreign[0].ends_with(".claude/settings.json: hooks.Stop"));
        assert!(
            c.short_timeouts(["Stop"].into_iter(), 20.0).is_empty(),
            "30 s is enough for a 20 s relay"
        );
        assert_eq!(c.short_timeouts(["Stop"].into_iter(), 25.0).len(), 2);
    }

    #[test]
    fn hooks_without_a_command_count_as_others() {
        let t = tempfile::tempdir().unwrap();
        let cwd = t.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let claude = builtin("claude");
        let mut m = serde_json::Map::new();
        for e in claude.events() {
            m.insert(
                e.to_string(),
                serde_json::json!([{"hooks": [{"type": "command",
                    "command": hook_command("claude", e), "timeout": 30}]}]),
            );
        }
        let mut v = serde_json::json!({ "hooks": m });
        let mut add = |event: &str, hook: Value| {
            v["hooks"][event]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({ "hooks": [hook] }));
        };
        add(
            "Stop",
            serde_json::json!({"type": "prompt", "prompt": "Done?"}),
        );
        add(
            "UserPromptSubmit",
            serde_json::json!({"type": "http", "url": "http://127.0.0.1:9/check"}),
        );
        // Not on an event that can block.
        add(
            "PreToolUse",
            serde_json::json!({"type": "agent", "prompt": "Safe?"}),
        );
        // Neither a command nor a type: not a hook.
        add("Stop", serde_json::json!({"timeout": 5}));
        write(&cwd.join(".claude/settings.json"), &v.to_string());
        let c = Check::run(&claude, &lookup(t.path()), &cwd, &["claude".into()]).unwrap();
        assert!(c.missing().is_empty());
        assert!(c.duplicated().is_empty());
        assert_eq!(c.foreign.len(), 2, "{:?}", c.foreign);
        assert!(
            c.foreign
                .iter()
                .any(|f| f.ends_with(".claude/settings.json: hooks.Stop (prompt)")),
            "{:?}",
            c.foreign
        );
        assert!(
            c.foreign
                .iter()
                .any(|f| f.ends_with(".claude/settings.json: hooks.UserPromptSubmit (http)")),
            "{:?}",
            c.foreign
        );
    }

    #[test]
    fn plugins_of_claude() {
        let t = tempfile::tempdir().unwrap();
        let cwd = t.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let plugin = t.path().join("plugin");
        write(
            &plugin.join("hooks/hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"review"}]}]}}"#,
        );
        write(
            &t.path().join("claude/settings.json"),
            r#"{"enabledPlugins":{"p@m":true}}"#,
        );
        write(
            &t.path().join("claude/plugins/installed_plugins.json"),
            &serde_json::json!({"plugins": {"p@m": [{"installPath": plugin}]}}).to_string(),
        );
        let found = find(
            &builtin("claude"),
            &lookup(t.path()),
            &cwd,
            &["claude".into()],
        )
        .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].place.starts_with("plugin p@m ("));
    }
}
