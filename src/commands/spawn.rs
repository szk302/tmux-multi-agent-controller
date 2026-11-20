//! `tmac spawn` (§5).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config;
use crate::driver::{Driver, PrepareCtx};
use crate::error::{Error, Result};
use crate::home::Home;
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
    pub human_hold: Option<f64>,
    pub allow_foreign_hooks: bool,
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
    let driver = match &a.driver {
        Some(d) => {
            Driver::from_name(d).ok_or_else(|| Error::usage(format!("unknown driver {d:?}")))?
        }
        None => Driver::from_command(&a.command[0]).ok_or_else(|| {
            Error::usage(format!(
                "cannot tell the driver from {:?}; pass --driver claude or --driver codex",
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

    let home = Home::resolve()?;
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
    let session = a
        .session
        .clone()
        .or_else(|| cfg.tmux_session.clone())
        .unwrap_or_else(|| project::session_name(&key.project));
    if session.is_empty() || session.contains([':', '.']) {
        return Err(Error::usage(format!(
            "invalid tmux session name {session:?}"
        )));
    }
    let human_hold = a.human_hold.unwrap_or(cfg.hold_human_sec);
    let timeout = a.timeout.unwrap_or(cfg.spawn_timeout);
    if human_hold < 0.0 || timeout < 0.0 {
        return Err(Error::usage("times must not be negative"));
    }

    let instance = uuid::Uuid::new_v4().hyphenated().to_string();
    let exe = std::env::current_exe()?.canonicalize()?;
    let driver_dir = home.agent_dir(&key).join("driver");
    let prepared = driver.prepare(
        &PrepareCtx {
            exe: &exe,
            project: &key.project,
            name: &key.name,
            instance: &instance,
            driver_dir: &driver_dir,
            cwd: &cwd,
            relay_timeout: cfg.relay_timeout,
        },
        &a.command,
    )?;
    if !a.allow_foreign_hooks {
        let found = driver.foreign_hooks(&cwd, &prepared);
        if !found.is_empty() {
            let mut msg = String::from(
                "hooks other than tmac's can block Stop or UserPromptSubmit, so tmac cannot tell the state \
                 reliably. Move them to --relay, or pass --allow-foreign-hooks if they never block:",
            );
            for f in found {
                msg.push_str("\n  ");
                msg.push_str(&f);
            }
            return Err(Error::Usage(msg));
        }
    }

    if let Some(old) = Agent::load(&home, &key)? {
        let exited = ops::reconcile(&old)
            .map(|(st, _)| st.state == State::Exited)
            .unwrap_or(false);
        let hint = if exited { " (it has exited)" } else { "" };
        return Err(Error::runtime(format!(
            "agent {key} already exists{hint}; run `tmac kill {key}` first"
        )));
    }

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
        command: prepared.command.clone(),
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
    let mut agent = record::create(&home, &info, &prepared.files)?;

    let tmux = Tmux::with_socket_name(cfg.tmux_socket_name.as_deref());
    let (pane, socket) = match create_pane(&tmux, &agent.info, &cwd, &env_all, &prepared.command) {
        Ok(v) => v,
        Err(e) => {
            let _ = record::remove(&agent);
            return Err(e);
        }
    };
    agent.info.tmux.pane = Some(pane.clone());
    agent.info.tmux.socket = Some(socket);
    agent.write_info()?;
    ops::refresh_display_now(&agent);

    let sig = util::signal_flag();
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    loop {
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
            return Err(Error::Timeout(format!(
                "{key} is still {} after {timeout}s; look at the pane (a confirmation screen?) \
                 or wait with `tmac wait {key}`",
                st.state.as_str()
            )));
        }
        util::sleep_checked(Duration::from_millis(500), &sig)?;
    }
}

/// Create the window, tag the pane and name the window in one tmux call
/// (§5.3). Returns the pane id and the server's socket path.
fn create_pane(
    tmux: &Tmux,
    info: &AgentFile,
    cwd: &std::path::Path,
    env: &[(String, String)],
    command: &[String],
) -> Result<(String, String)> {
    let session = &info.tmux.session;
    let instance = &info.instance;
    let target = format!("={session}:={instance}");
    let mut last_err = None;
    for _ in 0..3 {
        let mut args: Vec<String> = if tmux.has_session(session)? {
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
