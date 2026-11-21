//! `tmac doctor` and `tmac hooks rulesync` (§5.5, §5.6): install and check
//! tmac's hooks, which live in the user's CLI settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config;
use crate::driver::{self, Driver};
use crate::error::{Error, Result, warn};
use crate::home::Home;
use crate::hookconf::{self, Check, Dirs};
use crate::util;

fn drivers(names: &[String]) -> Result<Vec<Driver>> {
    if names.is_empty() {
        return Ok(driver::ALL.to_vec());
    }
    names
        .iter()
        .map(|n| Driver::from_name(n).ok_or_else(|| Error::usage(format!("unknown driver {n:?}"))))
        .collect()
}

/// Check the hooks for each driver without starting an agent. Exit code 0
/// when an agent could be started, 1 otherwise.
pub fn doctor(names: &[String], cwd: Option<&Path>) -> Result<bool> {
    let cwd = match cwd {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let cwd = cwd
        .canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| Error::usage(format!("not a directory: {}", cwd.display())))?;
    let home = Home::resolve()?;
    let cfg = config::load(&home)?;
    let dirs = Dirs::from_env();
    let mut ok = true;
    // What to suggest: installing tmac's hooks, or dealing with others.
    let mut need_install = false;
    let mut need_relay = false;
    let mut on_path = None;
    for d in drivers(names)? {
        let name = d.name();
        let c = Check::run(d, &dirs, &cwd, &[name.to_string()])?;
        on_path = c.tmac_on_path.clone();
        let missing = c.missing();
        let dup = c.duplicated();
        if missing.is_empty() && dup.is_empty() {
            let mut places: Vec<&str> = c
                .tmac
                .values()
                .flatten()
                .map(|f| f.place.as_str())
                .collect();
            places.sort();
            places.dedup();
            println!(
                "{name}: tmac's hooks: ok ({} events in {})",
                c.tmac.len(),
                places.join(", ")
            );
        } else {
            ok = false;
            need_install = true;
            if !missing.is_empty() {
                println!("{name}: tmac's hooks: missing {}", missing.join(", "));
            }
            for (ev, places) in dup {
                println!(
                    "{name}: tmac's hooks: {ev} is installed more than once ({})",
                    places.join(", ")
                );
            }
        }
        let relayed = cfg.relay.get(name).cloned().unwrap_or_default();
        let short = c.short_timeouts(relayed.keys().map(String::as_str), cfg.relay_timeout);
        for (ev, t) in &short {
            ok = false;
            println!(
                "{name}: time limit of the {ev} hook is {t}s, less than relay.timeout + 10 = {}s",
                cfg.relay_timeout + 10.0
            );
        }
        if c.foreign.is_empty() {
            println!("{name}: other hooks that can block: none");
        }
        for f in &c.foreign {
            if cfg.allow_foreign_hooks.allows(f) {
                println!(
                    "{name}: other hooks that can block: {f} (allowed by spawn.allow_foreign_hooks)"
                );
            } else {
                ok = false;
                need_relay = true;
                println!("{name}: other hooks that can block: {f}");
            }
        }
    }
    match on_path {
        Some(p) => println!("tmac in PATH: {}", p.display()),
        None => {
            ok = false;
            println!("tmac in PATH: not found (the hooks would do nothing)");
        }
    }
    if need_install {
        eprintln!("tmac: {}", hookconf::INSTALL_HINT);
    }
    if need_relay {
        eprintln!(
            "tmac: other hooks on Stop or UserPromptSubmit can block, so tmac cannot tell the \
             state reliably; move them to a relay (--relay, relay.<driver>.<event>), or list \
             them in spawn.allow_foreign_hooks if they never block"
        );
    }
    Ok(ok)
}

/// Remove `//` and `/* */` comments and trailing commas from JSONC.
pub fn strip_jsonc(text: &str) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            '/' if b.get(i + 1) == Some(&'/') => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if b.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                // Drop a comma that only has whitespace or comments before
                // the closing bracket.
                let mut j = i + 1;
                loop {
                    while j < b.len() && b[j].is_whitespace() {
                        j += 1;
                    }
                    if b.get(j) == Some(&'/') && b.get(j + 1) == Some(&'/') {
                        while j < b.len() && b[j] != '\n' {
                            j += 1;
                        }
                    } else if b.get(j) == Some(&'/') && b.get(j + 1) == Some(&'*') {
                        j += 2;
                        while j + 1 < b.len() && !(b[j] == '*' && b[j + 1] == '/') {
                            j += 1;
                        }
                        j += 2;
                    } else {
                        break;
                    }
                }
                if !matches!(b.get(j), Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn object<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<&'a mut Map<String, Value>> {
    let v = parent
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    v.as_object_mut()
        .ok_or_else(|| Error::usage(format!("{what} must be an object")))
}

/// The rulesync sources for tmac's hooks: `hooks.jsonc` (merged into
/// `base`, the user's own hooks) and `rulesync.jsonc`.
pub fn rulesync_sources(base: Option<Value>, extra_targets: &[String]) -> Result<(Value, Value)> {
    let mut root = match base.unwrap_or_else(|| json!({})) {
        Value::Object(m) => m,
        _ => return Err(Error::usage("the --merge file must be a JSON object")),
    };
    root.entry("version").or_insert(json!(1));
    object(&mut root, "hooks", "hooks")?;
    let mut targets: Vec<String> = Vec::new();
    for d in driver::ALL {
        let target = d.rulesync_target();
        targets.push(target.to_string());
        let section = object(&mut root, target, target)?;
        let hooks = object(section, "hooks", &format!("{target}.hooks"))?;
        for &event in d.events() {
            let list = hooks
                .entry(event.to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            let list = list
                .as_array_mut()
                .ok_or_else(|| Error::usage(format!("{target}.hooks.{event} must be an array")))?;
            let has = list.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .and_then(hookconf::tmac_hook_target)
                    .is_some_and(|(dn, e)| dn == d.name() && e == event)
            });
            if has {
                warn(format!(
                    "the --merge file already has tmac's {} {event} hook; kept it",
                    d.name()
                ));
                continue;
            }
            list.push(json!({
                "command": hookconf::hook_command(d, event),
                "timeout": d.hook_timeout(event),
            }));
        }
    }
    for t in extra_targets {
        if !targets.contains(t) {
            targets.push(t.clone());
        }
    }
    let config = json!({
        "targets": targets,
        "features": ["hooks"],
        "preserveUnownedHooks": true,
    });
    Ok((Value::Object(root), config))
}

/// `tmac hooks rulesync DIR`: write the rulesync sources that install
/// tmac's hooks for the user. tmac does not run rulesync.
pub fn rulesync(dir: &Path, merge: Option<&Path>, extra_targets: &[String]) -> Result<()> {
    let base = match merge {
        None => None,
        Some(f) => {
            let text = std::fs::read_to_string(f)
                .map_err(|e| Error::usage(format!("cannot read {}: {e}", f.display())))?;
            Some(
                serde_json::from_str::<Value>(&strip_jsonc(&text))
                    .map_err(|e| Error::usage(format!("cannot parse {}: {e}", f.display())))?,
            )
        }
    };
    let (hooks, config) = rulesync_sources(base, extra_targets)?;
    std::fs::create_dir_all(dir)?;
    let header = format!(
        "// Written by `tmac hooks rulesync` (tmac {}). Do not edit: write your own hooks in\n\
         // another file and pass it with --merge, then run rulesync again.\n",
        env!("CARGO_PKG_VERSION")
    );
    let files: BTreeMap<PathBuf, String> = [
        (
            dir.join("hooks.jsonc"),
            format!("{header}{}\n", pretty(&hooks)?),
        ),
        (
            dir.join("rulesync.jsonc"),
            format!("{header}{}\n", pretty(&config)?),
        ),
    ]
    .into();
    for (path, text) in &files {
        util::write_atomic(path, text.as_bytes())?;
        println!("{}", path.display());
    }
    Ok(())
}

fn pretty(v: &Value) -> Result<String> {
    serde_json::to_string_pretty(v).map_err(|e| Error::runtime(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc() {
        let text = "// head\n{\n  \"a\": \"x // not a comment\", /* c */\n  \"b\": [1, 2,],\n}\n";
        let v: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(v, json!({"a": "x // not a comment", "b": [1, 2]}));
    }

    #[test]
    fn sources_have_every_event() {
        let (h, c) = rulesync_sources(None, &[]).unwrap();
        assert_eq!(h["hooks"], json!({}));
        let stop = &h["claudecode"]["hooks"]["Stop"][0];
        assert_eq!(
            stop["command"],
            "command -v tmac >/dev/null 2>&1 || exit 0; exec tmac hook claude Stop"
        );
        assert_eq!(stop["timeout"], 30);
        assert_eq!(h["codexcli"]["hooks"]["Interrupt"][0]["timeout"], 3);
        assert_eq!(
            h["claudecode"]["hooks"].as_object().unwrap().len(),
            Driver::Claude.events().len()
        );
        assert_eq!(c["targets"], json!(["claudecode", "codexcli"]));
        assert_eq!(c["preserveUnownedHooks"], true);
    }

    #[test]
    fn sources_merge_the_users_hooks() {
        let base = json!({
            "version": 1,
            "hooks": {"sessionStart": [{"command": "echo mine"}]},
            "claudecode": {"hooks": {"Stop": [{"command": "echo stop"}]}},
        });
        let (h, c) = rulesync_sources(Some(base), &["cursor".into()]).unwrap();
        assert_eq!(h["hooks"]["sessionStart"][0]["command"], "echo mine");
        let stop = h["claudecode"]["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["command"], "echo stop");
        assert_eq!(c["targets"], json!(["claudecode", "codexcli", "cursor"]));
        // Merging the output again does not add tmac's hooks twice.
        let (again, _) = rulesync_sources(Some(h.clone()), &[]).unwrap();
        assert_eq!(again, h);
    }
}
