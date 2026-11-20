//! Configuration file (§12).

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

use serde_yaml_ng::Value;

use crate::error::{Error, Result, warn};
use crate::home::Home;

/// Settings with built-in defaults applied.
#[derive(Debug, Clone)]
pub struct Config {
    /// `None`: a session per project (§12.2).
    pub tmux_session: Option<String>,
    pub tmux_socket_name: Option<String>,
    pub spawn_timeout: f64,
    /// Other hooks that spawn accepts (§5.6).
    pub allow_foreign_hooks: ForeignHooks,
    pub send_settle_sec: f64,
    pub send_confirm_timeout: f64,
    pub send_max_bytes: usize,
    pub kill_timeout: f64,
    pub hold_human_sec: f64,
    pub relay_timeout: f64,
    /// driver -> event -> command
    pub relay: BTreeMap<String, BTreeMap<String, String>>,
    /// driver -> delay between paste and Enter, in milliseconds
    pub submit_delay_ms: BTreeMap<String, u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            tmux_session: None,
            tmux_socket_name: None,
            spawn_timeout: 60.0,
            allow_foreign_hooks: ForeignHooks::None,
            send_settle_sec: 10.0,
            send_confirm_timeout: 15.0,
            send_max_bytes: 65536,
            kill_timeout: 30.0,
            hold_human_sec: 300.0,
            relay_timeout: 20.0,
            relay: BTreeMap::new(),
            submit_delay_ms: BTreeMap::new(),
        }
    }
}

/// Hooks other than tmac's that `spawn` accepts without
/// `--allow-foreign-hooks` (§5.6, §12.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForeignHooks {
    None,
    All,
    /// Places (as spawn reports them) starting with one of these.
    Prefixes(Vec<String>),
}

impl ForeignHooks {
    pub fn allows(&self, place: &str) -> bool {
        match self {
            ForeignHooks::None => false,
            ForeignHooks::All => true,
            ForeignHooks::Prefixes(p) => p.iter().any(|p| place.starts_with(p.as_str())),
        }
    }
}

impl Config {
    /// Delay between paste and Enter for a driver.
    pub fn submit_delay(&self, driver: &str, default_ms: u64) -> u64 {
        self.submit_delay_ms
            .get(driver)
            .copied()
            .unwrap_or(default_ms)
    }
}

/// Path of the configuration file and whether it was given explicitly.
pub fn config_path(home: &Home) -> (PathBuf, bool) {
    match env::var_os("TMAC_CONFIG").filter(|v| !v.is_empty()) {
        Some(p) => (PathBuf::from(p), true),
        None => (home.root.join("config.yml"), false),
    }
}

/// Load the configuration (§12.1).
pub fn load(home: &Home) -> Result<Config> {
    let (path, explicit) = config_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
            return Ok(Config::default());
        }
        Err(e) => {
            return Err(Error::usage(format!(
                "cannot read config {}: {e}",
                path.display()
            )));
        }
    };
    parse(&text).map_err(|e| Error::usage(format!("{}: {e}", path.display())))
}

fn number(v: &Value, key: &str) -> std::result::Result<f64, String> {
    match v.as_f64() {
        Some(n) if n >= 0.0 && n.is_finite() => Ok(n),
        _ => Err(format!("{key} must be a non-negative number")),
    }
}

fn string(v: &Value, key: &str) -> std::result::Result<String, String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn mapping<'a>(v: &'a Value, key: &str) -> std::result::Result<&'a serde_yaml_ng::Mapping, String> {
    v.as_mapping()
        .ok_or_else(|| format!("{key} must be a mapping"))
}

/// Parse configuration text. Unknown keys produce warnings.
pub fn parse(text: &str) -> std::result::Result<Config, String> {
    let mut cfg = Config::default();
    let root: Value = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    let root = match &root {
        Value::Null => return Ok(cfg),
        v => mapping(v, "the configuration")?,
    };
    for (k, v) in root {
        let section = k.as_str().unwrap_or_default();
        if v.is_null() {
            continue;
        }
        match section {
            "tmux" => {
                for (k2, v2) in mapping(v, "tmux")? {
                    match k2.as_str().unwrap_or_default() {
                        "session" => cfg.tmux_session = Some(string(v2, "tmux.session")?),
                        "socket_name" => {
                            cfg.tmux_socket_name = Some(string(v2, "tmux.socket_name")?)
                        }
                        other => warn(format!("unknown config key: tmux.{other}")),
                    }
                }
            }
            "spawn" => {
                for (k2, v2) in mapping(v, "spawn")? {
                    match k2.as_str().unwrap_or_default() {
                        "timeout" => cfg.spawn_timeout = number(v2, "spawn.timeout")?,
                        "allow_foreign_hooks" => {
                            cfg.allow_foreign_hooks = match v2 {
                                Value::Bool(true) => ForeignHooks::All,
                                Value::Bool(false) => ForeignHooks::None,
                                Value::Sequence(items) => ForeignHooks::Prefixes(
                                    items
                                        .iter()
                                        .map(|i| string(i, "spawn.allow_foreign_hooks[]"))
                                        .collect::<std::result::Result<_, _>>()?,
                                ),
                                _ => {
                                    return Err(
                                        "spawn.allow_foreign_hooks must be true, false or a list of strings"
                                            .into(),
                                    );
                                }
                            }
                        }
                        other => warn(format!("unknown config key: spawn.{other}")),
                    }
                }
            }
            "send" => {
                for (k2, v2) in mapping(v, "send")? {
                    match k2.as_str().unwrap_or_default() {
                        "settle_sec" => cfg.send_settle_sec = number(v2, "send.settle_sec")?,
                        "confirm_timeout" => {
                            cfg.send_confirm_timeout = number(v2, "send.confirm_timeout")?
                        }
                        "max_bytes" => {
                            cfg.send_max_bytes = v2
                                .as_u64()
                                .filter(|&n| n > 0)
                                .ok_or("send.max_bytes must be a positive integer")?
                                as usize
                        }
                        other => warn(format!("unknown config key: send.{other}")),
                    }
                }
            }
            "kill" => {
                for (k2, v2) in mapping(v, "kill")? {
                    match k2.as_str().unwrap_or_default() {
                        "timeout" => cfg.kill_timeout = number(v2, "kill.timeout")?,
                        other => warn(format!("unknown config key: kill.{other}")),
                    }
                }
            }
            "hold" => {
                for (k2, v2) in mapping(v, "hold")? {
                    match k2.as_str().unwrap_or_default() {
                        "human_sec" => cfg.hold_human_sec = number(v2, "hold.human_sec")?,
                        other => warn(format!("unknown config key: hold.{other}")),
                    }
                }
            }
            "relay" => {
                for (k2, v2) in mapping(v, "relay")? {
                    let k2 = k2.as_str().unwrap_or_default();
                    if k2 == "timeout" {
                        cfg.relay_timeout = number(v2, "relay.timeout")?;
                        continue;
                    }
                    if crate::driver::Driver::from_name(k2).is_none() {
                        warn(format!("unknown config key: relay.{k2}"));
                        continue;
                    }
                    let mut events = BTreeMap::new();
                    for (ev, cmd) in mapping(v2, &format!("relay.{k2}"))? {
                        let ev = ev.as_str().unwrap_or_default().to_string();
                        events.insert(ev.clone(), string(cmd, &format!("relay.{k2}.{ev}"))?);
                    }
                    cfg.relay.insert(k2.to_string(), events);
                }
            }
            "drivers" => {
                for (k2, v2) in mapping(v, "drivers")? {
                    let d = k2.as_str().unwrap_or_default();
                    if crate::driver::Driver::from_name(d).is_none() {
                        warn(format!("unknown config key: drivers.{d}"));
                        continue;
                    }
                    for (k3, v3) in mapping(v2, &format!("drivers.{d}"))? {
                        match k3.as_str().unwrap_or_default() {
                            "submit_delay_ms" => {
                                let n = v3.as_u64().ok_or(format!(
                                    "drivers.{d}.submit_delay_ms must be a non-negative integer"
                                ))?;
                                cfg.submit_delay_ms.insert(d.to_string(), n);
                            }
                            other => warn(format!("unknown config key: drivers.{d}.{other}")),
                        }
                    }
                }
            }
            other => warn(format!("unknown config key: {other}")),
        }
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        let c = parse("").unwrap();
        assert_eq!(c.tmux_session, None);
        assert_eq!(c.send_settle_sec, 10.0);
        let c = parse(
            "tmux:\n  session: agents\nrelay:\n  timeout: 5\n  claude:\n    Stop: lmm hook\nsend:\n  settle_sec: 30\nhold:\n  human_sec: 600\ndrivers:\n  codex:\n    submit_delay_ms: 700\n",
        )
        .unwrap();
        assert_eq!(c.tmux_session.as_deref(), Some("agents"));
        assert_eq!(c.relay_timeout, 5.0);
        assert_eq!(c.relay["claude"]["Stop"], "lmm hook");
        assert_eq!(c.send_settle_sec, 30.0);
        assert_eq!(c.hold_human_sec, 600.0);
        assert_eq!(c.submit_delay("codex", 1), 700);
        assert_eq!(c.submit_delay("claude", 200), 200);
    }

    #[test]
    fn foreign_hooks_setting() {
        assert_eq!(parse("").unwrap().allow_foreign_hooks, ForeignHooks::None);
        let c = parse("spawn:\n  allow_foreign_hooks: true\n").unwrap();
        assert!(c.allow_foreign_hooks.allows("anything"));
        let c = parse("spawn:\n  allow_foreign_hooks:\n    - plugin codex@openai-codex\n").unwrap();
        assert!(
            c.allow_foreign_hooks
                .allows("plugin codex@openai-codex (/x/hooks.json): hooks.Stop")
        );
        assert!(
            !c.allow_foreign_hooks
                .allows("/repo/.claude/settings.json: hooks.Stop")
        );
        assert!(parse("spawn:\n  allow_foreign_hooks: 3\n").is_err());
    }

    #[test]
    fn errors() {
        assert!(parse("send:\n  settle_sec: -1\n").is_err());
        assert!(parse("tmux: 3\n").is_err());
        assert!(parse("- a\n").is_err());
    }
}
