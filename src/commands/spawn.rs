//! `tmac spawn` (§5).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config;
use crate::driver::Driver;
use crate::error::{Error, Result};
use crate::home::{Home, LockKind};
use crate::hookconf::{self, Check, Lookup};
use crate::ops;
use crate::project::{self, AgentRef};
use crate::record::{self, Agent, AgentFile, State, TmuxInfo};
use crate::tmux::{Tmux, escape_arg};
use crate::util;

pub struct SpawnArgs {
    pub project: Option<String>,
    pub name: String,
    pub driver: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<String>,
    pub pass_env: Vec<String>,
    pub relay: Vec<String>,
    pub session: Option<String>,
    /// Fail instead of creating the session (§5.2).
    pub no_create_session: bool,
    pub human_hold: Option<f64>,
    pub allow_foreign_hooks: bool,
    /// Instead of the driver's first prompt.
    pub first_prompt: Option<String>,
    pub no_first_prompt: bool,
    pub timeout: Option<f64>,
    pub command: Vec<String>,
}

const RESERVED_ENV: &[&str] = &["TMAC_PROJECT", "TMAC_AGENT", "TMAC_INSTANCE", "TMAC_HOME"];

fn valid_env_key(k: &str) -> bool {
    let b = k.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

pub fn run(a: SpawnArgs) -> Result<()> {
    if a.command.is_empty() {
        return Err(Error::usage("give the command after --"));
    }
    let home = Home::resolve()?;
    let driver = match &a.driver {
        Some(d) => Driver::load(&home, d)?,
        None => Driver::from_command(&home, &a.command[0])?.ok_or_else(|| {
            let names = Driver::names(&home).unwrap_or_default().join(", ");
            Error::usage(format!(
                "cannot tell the driver from {:?}; pass --driver (drivers: {names})",
                a.command[0]
            ))
        })?,
    };
    let cwd = match &a.cwd {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };
    let cwd = cwd
        .canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| Error::usage(format!("not a directory: {}", cwd.display())))?;
    // The project of an agent is the repository it works in (§3.1).
    let key = AgentRef::parse(&a.name, || project::resolve(a.project.as_deref(), &cwd))?;

    let mut env_pairs: Vec<(String, String)> = Vec::new();
    for kv in &a.env {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| Error::usage(format!("--env takes KEY=VALUE: {kv:?}")))?;
        if !valid_env_key(k) || RESERVED_ENV.contains(&k) {
            return Err(Error::usage(format!(
                "cannot set the environment variable {k:?}"
            )));
        }
        env_pairs.push((k.into(), v.into()));
    }
    for k in &a.pass_env {
        if !valid_env_key(k) || RESERVED_ENV.contains(&k.as_str()) {
            return Err(Error::usage(format!(
                "cannot pass the environment variable {k:?}"
            )));
        }
        let v = std::env::var(k)
            .map_err(|_| Error::usage(format!("--pass-env {k}: the variable is not set")))?;
        env_pairs.push((k.clone(), v));
    }

    let cfg = config::load(&home)?;
    let mut relay: BTreeMap<String, String> =
        cfg.relay.get(driver.name()).cloned().unwrap_or_default();
    for r in &a.relay {
        let (ev, cmd) = r
            .split_once('=')
            .ok_or_else(|| Error::usage(format!("--relay takes EVENT=COMMAND: {r:?}")))?;
        if cmd.trim().is_empty() {
            relay.remove(ev);
        } else {
            relay.insert(ev.into(), cmd.into());
        }
    }
    for ev in relay.keys() {
        if !driver.events().contains(&ev.as_str()) {
            return Err(Error::usage(format!(
                "the {} driver has no {ev:?} hook to relay (events: {})",
                driver.name(),
                driver.events().join(", ")
            )));
        }
    }
    let session = match &a.session {
        Some(s) => project::check_session(s)?,
        None => project::session_for(&cfg, &key.project)?,
    };
    let tmux = Tmux::with_socket_name(cfg.tmux_socket_name.as_deref());
    let session_exists = tmux.has_session(&session)?;
    if a.no_create_session && !session_exists {
        return Err(Error::runtime(format!(
            "the tmux session {session:?} does not exist; create it from your terminal with \
             `tmac session --create` (agents get the environment of the session)"
        )));
    }
    let human_hold = a.human_hold.unwrap_or(cfg.hold_human_sec);
    let timeout = a.timeout.unwrap_or(cfg.spawn_timeout);
    if human_hold < 0.0 || timeout < 0.0 {
        return Err(Error::usage("times must not be negative"));
    }

    // The command as the CLI gets it: with the first prompt the driver
    // needs, if any (§5.3).
    let mut command = a.command.clone();
    if !a.no_first_prompt
        && let Some(p) = a.first_prompt.as_deref().or(driver.first_prompt())
    {
        command.push(p.to_string());
    }
    // Look for the hooks where the agent will: with the environment of the
    // session it starts in, not the caller's (§5.6).
    let lookup = agent_lookup(
        &tmux,
        session_exists.then_some(session.as_str()),
        &env_pairs,
    )?;
    check_hooks(
        &driver,
        &lookup,
        &cwd,
        &command,
        &relay,
        &cfg,
        a.allow_foreign_hooks,
    )?;
    // Remove the variables CLIs set for their own sessions (§5.4): every
    // driver's, since the parent CLI may be of any kind.
    let mut unset: Vec<String> = Vec::new();
    for d in Driver::all(&home)? {
        for v in d.session_env() {
            if !unset.contains(v) {
                unset.push(v.clone());
            }
        }
    }
    let mut pane_command = Vec::new();
    if !unset.is_empty() {
        pane_command.push(std::env::current_exe()?.to_string_lossy().into_owned());
        pane_command.push("exec-clean".into());
        for v in &unset {
            pane_command.push("--unset".into());
            pane_command.push(v.clone());
        }
        pane_command.push("--".into());
    }
    pane_command.extend(command.iter().cloned());

    // Hold the input lock from creating the record until the pane is in it
    // (§4.3, §5.3): kill takes the same lock, so it cannot remove the
    // record in between and leave a pane that has no record.
    let sig = util::signal_flag();
    let input_lock = home.lock(&key, LockKind::Input, None, Some(&sig))?;
    if let Some(old) = Agent::load(&home, &key)? {
        let exited = ops::reconcile(&old)
            .map(|(st, _)| st.state == State::Exited)
            .unwrap_or(false);
        let hint = if exited { " (it has exited)" } else { "" };
        return Err(Error::runtime(format!(
            "agent {key} already exists{hint}; run `tmac kill {key}` first"
        )));
    }

    let instance = uuid::Uuid::new_v4().hyphenated().to_string();
    let mut env_all: Vec<(String, String)> = vec![
        ("TMAC_PROJECT".into(), key.project.clone()),
        ("TMAC_AGENT".into(), key.name.clone()),
        ("TMAC_INSTANCE".into(), instance.clone()),
        ("TMAC_HOME".into(), home.root.to_string_lossy().into_owned()),
    ];
    if let (path, true) = config::config_path(&home) {
        let abs = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        env_all.push(("TMAC_CONFIG".into(), abs.to_string_lossy().into_owned()));
    }
    env_all.extend(env_pairs.iter().cloned());

    let info = AgentFile {
        project: key.project.clone(),
        name: key.name.clone(),
        instance: instance.clone(),
        driver: driver.name().into(),
        created_at: util::now_stamp(),
        spawn_timeout: timeout,
        cwd: cwd.to_string_lossy().into_owned(),
        command: command.clone(),
        env_names: env_pairs.iter().map(|(k, _)| k.clone()).collect(),
        tmux: TmuxInfo {
            socket_name: cfg.tmux_socket_name.clone(),
            socket: None,
            session: session.clone(),
            pane: None,
        },
        relay,
        human_hold,
    };
    let mut agent = record::create(&home, &info)?;

    let created = create_pane(
        &tmux,
        &agent.info,
        &cwd,
        &env_all,
        &pane_command,
        !a.no_create_session,
    );
    let (pane, socket) = match created {
        Ok(v) => v,
        Err(e) => {
            let _ = record::remove(&agent);
            return Err(e);
        }
    };
    agent.info.tmux.pane = Some(pane.clone());
    agent.info.tmux.socket = Some(socket);
    if let Err(e) = agent.write_info() {
        // Do not leave a pane that has no record.
        let _ = Tmux::for_agent(&agent.info.tmux).kill_pane_tagged(&pane, &instance);
        let _ = record::remove(&agent);
        return Err(e);
    }
    ops::refresh_display_now(&agent);
    drop(input_lock);

    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    loop {
        if !agent.still_current()? {
            return Err(Error::NoAgent(format!(
                "{key} was removed during start-up (tmac kill?)"
            )));
        }
        let (st, _) = ops::reconcile(&agent)?;
        match st.state {
            State::Idle => {
                println!("{key} {pane}");
                return Ok(());
            }
            State::Exited => {
                return Err(Error::runtime(format!(
                    "{key} exited during start-up; look at the pane, then run `tmac kill {key}`"
                )));
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            let hint = driver
                .timeout_hint()
                .unwrap_or("look at the pane (a confirmation screen?).");
            return Err(Error::Timeout(format!(
                "{key} is still {} after {timeout}s; {hint} Then wait with `tmac wait {key}`",
                st.state.as_str()
            )));
        }
        util::sleep_checked(Duration::from_millis(500), &sig)?;
    }
}

/// The environment the agent will get, to find the CLI's settings (§5.6):
/// the tmux server's global environment with the session's on top (or this
/// process's, if no server runs: spawn starts one with it), and the
/// variables spawn adds.
fn agent_lookup(tmux: &Tmux, session: Option<&str>, added: &[(String, String)]) -> Result<Lookup> {
    let mut vars = match tmux.environment(session)? {
        Some(env) => env,
        None => std::env::vars().collect(),
    };
    vars.extend(added.iter().cloned());
    Ok(Lookup::from_map(vars))
}

/// Check the hooks the CLI will run before starting it (§5.6): tmac's hook
/// once for every event, `tmac` in PATH, time limits long enough for the
/// relays, and no other hooks that can block.
fn check_hooks(
    driver: &Driver,
    lookup: &Lookup,
    cwd: &std::path::Path,
    command: &[String],
    relay: &BTreeMap<String, String>,
    cfg: &config::Config,
    allow_foreign: bool,
) -> Result<()> {
    let c = Check::run(driver, lookup, cwd, command)?;
    let missing = c.missing();
    if !missing.is_empty() {
        return Err(Error::usage(format!(
            "tmac's {} hooks are not installed (missing: {}); {}",
            driver.name(),
            missing.join(", "),
            hookconf::INSTALL_HINT
        )));
    }
    let dup = c.duplicated();
    if !dup.is_empty() {
        let mut msg = format!(
            "tmac's {} hooks are installed more than once, so each event would be counted twice; \
             keep one of each:",
            driver.name()
        );
        for (ev, places) in dup {
            msg.push_str(&format!("\n  {ev}: {}", places.join(", ")));
        }
        return Err(Error::Usage(msg));
    }
    if c.tmac_on_path.is_none() {
        return Err(Error::usage(
            "tmac is not in PATH, so the hooks would do nothing; add its directory to PATH \
             (the agent gets the PATH of this command)",
        ));
    }
    let short = c.short_timeouts(relay.keys().map(String::as_str), cfg.relay_timeout);
    if !short.is_empty() {
        let list: Vec<String> = short.iter().map(|(e, t)| format!("{e} ({t}s)")).collect();
        return Err(Error::usage(format!(
            "the time limits of tmac's hooks must be at least relay.timeout + 10 = {}s for the \
             relayed events: {}",
            cfg.relay_timeout + 10.0,
            list.join(", ")
        )));
    }
    if !allow_foreign {
        let found: Vec<&String> = c
            .foreign
            .iter()
            .filter(|place| !cfg.allow_foreign_hooks.allows(place))
            .collect();
        if !found.is_empty() {
            let mut msg = String::from(
                "hooks other than tmac's can block Stop or UserPromptSubmit, so tmac cannot tell the state \
                 reliably. Move them to --relay, or pass --allow-foreign-hooks (or list them in \
                 spawn.allow_foreign_hooks of the configuration) if they never block:",
            );
            for f in found {
                msg.push_str("\n  ");
                msg.push_str(f);
            }
            return Err(Error::Usage(msg));
        }
    }
    Ok(())
}

/// Create the window, tag the pane and name the window in one tmux call
/// (§5.3). Returns the pane id and the server's socket path.
fn create_pane(
    tmux: &Tmux,
    info: &AgentFile,
    cwd: &std::path::Path,
    env: &[(String, String)],
    command: &[String],
    may_create_session: bool,
) -> Result<(String, String)> {
    let session = &info.tmux.session;
    let instance = &info.instance;
    let target = format!("={session}:={instance}");
    let mut last_err = None;
    for _ in 0..3 {
        let exists = tmux.has_session(session)?;
        if !exists && !may_create_session {
            return Err(Error::runtime(format!(
                "the tmux session {session:?} is gone"
            )));
        }
        let mut args: Vec<String> = if exists {
            vec![
                "new-window".into(),
                "-d".into(),
                "-t".into(),
                format!("={session}:"),
            ]
        } else {
            vec![
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                session.clone(),
            ]
        };
        args.extend([
            "-n".into(),
            instance.clone(),
            "-c".into(),
            escape_arg(&cwd.to_string_lossy()),
        ]);
        for (k, v) in env {
            args.push("-e".into());
            args.push(escape_arg(&format!("{k}={v}")));
        }
        args.extend(["-P".into(), "-F".into(), "#{pane_id} #{socket_path}".into()]);
        args.extend(command.iter().map(|c| escape_arg(c)));
        for opt in [
            vec![
                "set-option",
                "-p",
                "-t",
                &target,
                "@tmac_instance",
                instance,
            ],
            vec!["set-option", "-p", "-t", &target, "@tmac_agent", &info.name],
            vec!["set-option", "-p", "-t", &target, "remain-on-exit", "on"],
            vec!["rename-window", "-t", &target, &info.name],
        ] {
            args.push(";".into());
            args.extend(opt.into_iter().map(String::from));
        }
        if !exists {
            // `new-session -e` puts the variables in the session's
            // environment, which later windows inherit: keep them for this
            // agent's process only (§5.4). The pane has already started.
            for (k, _) in env {
                args.extend([
                    ";".into(),
                    "set-environment".into(),
                    "-t".into(),
                    format!("={session}"),
                    "-u".into(),
                    escape_arg(k),
                ]);
            }
        }
        match tmux.run(&args) {
            Ok(out) => {
                let line = out.lines().next().unwrap_or("").trim();
                let (pane, socket) = line.split_once(' ').unwrap_or((line, ""));
                if !pane.starts_with('%') {
                    return Err(Error::runtime(format!("unexpected tmux output: {out:?}")));
                }
                return Ok((pane.to_string(), socket.to_string()));
            }
            // Another spawn created the session in between: use new-window.
            Err(e) if e.to_string().contains("duplicate session") => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::runtime("cannot create the tmux window")))
}
