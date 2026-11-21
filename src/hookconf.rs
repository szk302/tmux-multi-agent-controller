//! Hooks registered with the CLIs (§5.5, §5.6): where they are, which of
//! them are tmac's, and whether an agent can be started with them.
//!
//! tmac does not write the CLIs' settings. Its hooks are installed once for
//! the user (with rulesync, `tmac hooks rulesync`), and `spawn` and
//! `tmac doctor` read the settings to check them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::driver::{BLOCKING_EVENTS, Driver};
use crate::error::{Error, Result};

/// One hook command found in a place.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Where it is, as reported to the user (a file path, `--settings`,
    /// `plugin NAME (PATH)`, `command line`).
    pub place: String,
    pub event: String,
    pub command: String,
    /// Seconds, if the hook sets a time limit.
    pub timeout: Option<f64>,
}

/// The guarded command tmac's hooks run (§5.5): it does nothing where tmac
/// is not installed.
pub fn hook_command(driver: Driver, event: &str) -> String {
    format!(
        "command -v tmac >/dev/null 2>&1 || exit 0; exec tmac hook {} {event}",
        driver.name()
    )
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

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn read_toml(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let table = text.parse::<toml::Table>().ok()?;
    serde_json::to_value(table).ok()
}

/// Hook commands in a hooks object (`{"Stop": [{"hooks": [{...}]}]}`).
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
                let Some(command) = h.get("command").and_then(Value::as_str) else {
                    continue;
                };
                out.push(Found {
                    place: place.to_string(),
                    event: event.clone(),
                    command: command.to_string(),
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

fn env_dir(var: &str, default_in_home: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(default_in_home))
        })
}

/// Directories of the CLIs' settings.
#[derive(Debug, Clone)]
pub struct Dirs {
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude_config: Option<PathBuf>,
    /// `$CODEX_HOME`, else `~/.codex`.
    pub codex_home: Option<PathBuf>,
    /// Whether to read the system-wide settings (`/etc/...`).
    pub system: bool,
}

impl Dirs {
    pub fn from_env() -> Dirs {
        Dirs {
            claude_config: env_dir("CLAUDE_CONFIG_DIR", ".claude"),
            codex_home: env_dir("CODEX_HOME", ".codex"),
            system: true,
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

/// Every hook command the CLI would run for an agent in `cwd` started with
/// `command`.
pub fn find(driver: Driver, dirs: &Dirs, cwd: &Path, command: &[String]) -> Result<Vec<Found>> {
    let args = match command.get(cli_index(command, driver) + 1..) {
        Some(a) => a,
        None => &[],
    };
    match driver {
        Driver::Claude => find_claude(dirs, cwd, args),
        Driver::Codex => Ok(find_codex(dirs, cwd, args)),
    }
}

/// The caller's `--settings` value, if any.
fn settings_arg(args: &[String]) -> Option<&str> {
    let mut value = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--settings" && i + 1 < args.len() {
            value = Some(args[i + 1].as_str());
            i += 2;
            continue;
        }
        if let Some(v) = args[i].strip_prefix("--settings=") {
            value = Some(v);
        }
        i += 1;
    }
    value
}

fn find_claude(dirs: &Dirs, cwd: &Path, args: &[String]) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    let config_dir = dirs.claude_config.clone();
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
    if dirs.system {
        files.push(PathBuf::from(if cfg!(target_os = "macos") {
            "/Library/Application Support/ClaudeCode/managed-settings.json"
        } else {
            "/etc/claude-code/managed-settings.json"
        }));
    }
    let mut enabled: BTreeMap<String, bool> = BTreeMap::new();
    for f in &files {
        let Some(v) = read_json(f) else { continue };
        collect(&f.display().to_string(), v.get("hooks"), &mut found);
        if let Some(p) = v.get("enabledPlugins").and_then(Value::as_object) {
            for (k, on) in p {
                enabled.insert(k.clone(), on.as_bool().unwrap_or(false));
            }
        }
    }
    if let Some(v) = settings_arg(args) {
        let parsed: Value = if v.trim_start().starts_with('{') {
            serde_json::from_str(v)
                .map_err(|e| Error::usage(format!("cannot parse --settings JSON: {e}")))?
        } else {
            let path = cwd.join(v);
            let text = std::fs::read_to_string(&path).map_err(|e| {
                Error::usage(format!("cannot read --settings {}: {e}", path.display()))
            })?;
            serde_json::from_str(&text)
                .map_err(|e| Error::usage(format!("cannot parse {}: {e}", path.display())))?
        };
        collect("--settings", parsed.get("hooks"), &mut found);
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
                    collect(
                        &format!("plugin {plugin} ({})", file.display()),
                        hooks,
                        &mut found,
                    );
                }
            }
        }
    }
    Ok(found)
}

fn find_codex(where_: &Dirs, cwd: &Path, args: &[String]) -> Vec<Found> {
    let mut found = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if where_.system {
        dirs.push(PathBuf::from("/etc/codex"));
    }
    if let Some(h) = &where_.codex_home {
        dirs.push(h.clone());
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
            collect(
                &hooks_json.display().to_string(),
                v.get("hooks"),
                &mut found,
            );
        }
        let config = d.join("config.toml");
        if let Some(v) = read_toml(&config) {
            collect(&config.display().to_string(), v.get("hooks"), &mut found);
        }
    }
    // The caller's own -c hooks.EVENT=VALUE overrides.
    let mut i = 0;
    while i + 1 < args.len() {
        if args[i] == "-c" || args[i] == "--config" {
            if let Some((key, value)) = args[i + 1].split_once('=')
                && let Some(event) = key.trim().strip_prefix("hooks.")
                && let Ok(t) = format!("v = {value}").parse::<toml::Table>()
                && let Ok(v) = serde_json::to_value(&t["v"])
            {
                let mut hooks = serde_json::Map::new();
                hooks.insert(event.to_string(), v);
                collect("command line", Some(&Value::Object(hooks)), &mut found);
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    found
}

/// The result of checking the hooks for a driver (§5.6).
#[derive(Debug, Clone)]
pub struct Check {
    pub driver: Driver,
    /// tmac's hooks for each event of the driver.
    pub tmac: BTreeMap<String, Vec<Found>>,
    /// Places with other hooks on events that can block, as
    /// `PLACE: hooks.EVENT`.
    pub foreign: Vec<String>,
    /// `tmac` in the PATH the agent gets.
    pub tmac_on_path: Option<PathBuf>,
}

impl Check {
    pub fn run(driver: Driver, dirs: &Dirs, cwd: &Path, command: &[String]) -> Result<Check> {
        let found = find(driver, dirs, cwd, command)?;
        let mut tmac: BTreeMap<String, Vec<Found>> = driver
            .events()
            .iter()
            .map(|e| (e.to_string(), Vec::new()))
            .collect();
        let mut foreign = Vec::new();
        for f in found {
            match tmac_hook_target(&f.command) {
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
                        let line = format!("{}: hooks.{}", f.place, f.event);
                        if !foreign.contains(&line) {
                            foreign.push(line);
                        }
                    }
                }
            }
        }
        Ok(Check {
            driver,
            tmac,
            foreign,
            tmac_on_path: which("tmac"),
        })
    }

    /// Events without tmac's hook, in the driver's order.
    pub fn missing(&self) -> Vec<&'static str> {
        self.driver
            .events()
            .iter()
            .copied()
            .filter(|e| self.tmac.get(*e).is_none_or(Vec::is_empty))
            .collect()
    }

    /// Events with tmac's hook more than once, and their places.
    pub fn duplicated(&self) -> Vec<(&'static str, Vec<&str>)> {
        self.driver
            .events()
            .iter()
            .copied()
            .filter_map(|e| {
                let l = self.tmac.get(e)?;
                (l.len() > 1).then(|| (e, l.iter().map(|f| f.place.as_str()).collect()))
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

    #[test]
    fn tmac_hook_commands() {
        let t = |c: &str| tmac_hook_target(c).map(|(d, e)| format!("{d} {e}"));
        assert_eq!(
            t(&hook_command(Driver::Claude, "Stop")).as_deref(),
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

    fn test_dirs(root: &Path) -> Dirs {
        Dirs {
            claude_config: Some(root.join("claude")),
            codex_home: Some(root.join("codex")),
            system: false,
        }
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
                hook_command(Driver::Codex, "Stop")
            ),
        );
        write(
            &cwd.join(".codex/config.toml"),
            "[[hooks.UserPromptSubmit]]\nhooks = [{ type = \"command\", command = \"guard\" }]\n",
        );
        let args = vec![
            "-c".to_string(),
            "hooks.Stop=[{hooks=[{type=\"command\",command=\"other\"}]}]".to_string(),
        ];
        let found = find_codex(&test_dirs(t.path()), &cwd, &args);
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
        let hooks = |events: &[&str], extra: &str| {
            let mut m = serde_json::Map::new();
            for e in events {
                m.insert(
                    e.to_string(),
                    serde_json::json!([{"hooks": [{"type": "command",
                        "command": hook_command(Driver::Claude, e), "timeout": 30}]}]),
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
            &hooks(Driver::Claude.events(), "lmm hook"),
        );
        let settings = hooks(&["Stop"], "");
        let cmd: Vec<String> = ["claude", "--settings", settings.as_str()]
            .map(String::from)
            .to_vec();
        let c = Check::run(Driver::Claude, &test_dirs(t.path()), &cwd, &cmd).unwrap();
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
}
