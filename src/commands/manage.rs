//! `tmac wait` (§8), `hold` / `release` (§9), `status` / `list` (§10) and
//! `kill` (§11).

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config;
use crate::driver::Driver;
use crate::error::{Error, Result, warn};
use crate::home::{Home, LockKind};
use crate::ops;
use crate::project::AgentRef;
use crate::record::{self, Agent, HUMAN, HoldFile, State, UNCONFIRMED};
use crate::tmux::Tmux;
use crate::util;

// ---- wait ----

pub fn wait(agent_ref: &AgentRef, states: &[String], timeout: Option<f64>) -> Result<()> {
    let name = &agent_ref.to_string();
    let mut targets = Vec::new();
    for s in states.iter().flat_map(|s| s.split(',')) {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        targets.push(State::parse(s).ok_or_else(|| Error::usage(format!("unknown state {s:?}")))?);
    }
    if targets.is_empty() {
        targets.push(State::Idle);
    }
    let home = Home::resolve()?;
    let agent = Agent::require(&home, agent_ref)?;
    let sig = util::signal_flag();
    let deadline = timeout.map(|t| Instant::now() + Duration::from_secs_f64(t));
    loop {
        util::check_signal(&sig)?;
        if !agent.still_current()? {
            return Err(Error::NoAgent(format!(
                "agent {name} is gone or was replaced"
            )));
        }
        let (st, _) = ops::reconcile(&agent)?;
        if targets.contains(&st.state) {
            println!("{}", st.state.as_str());
            return Ok(());
        }
        if st.state == State::Exited {
            return Err(Error::NoAgent(format!("agent {name} has exited")));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Error::Timeout(format!(
                "{name} is still {}",
                st.state.as_str()
            )));
        }
        util::sleep_checked(Duration::from_millis(500), &sig)?;
    }
}

// ---- hold / release ----

pub fn hold(
    agent_ref: &AgentRef,
    key: Option<&str>,
    note: Option<&str>,
    timeout: Option<f64>,
) -> Result<()> {
    let name = &agent_ref.to_string();
    let key = key.unwrap_or("manual");
    if !util::valid_name(key) {
        return Err(Error::usage(format!("invalid hold key {key:?}")));
    }
    if key == HUMAN || key == UNCONFIRMED {
        return Err(Error::usage(format!("the hold key {key:?} is reserved")));
    }
    let home = Home::resolve()?;
    let cfg = config::load(&home)?;
    let agent = Agent::require(&home, agent_ref)?;
    let sig = util::signal_flag();
    let wait = timeout.unwrap_or(cfg.send_confirm_timeout + 60.0);
    // Waiting for the input lock lets any input in progress finish (§9.2).
    let _input = home.lock(
        agent_ref,
        LockKind::Input,
        Some(Duration::from_secs_f64(wait)),
        Some(&sig),
    )?;
    let _g = home.state_lock(agent_ref)?;
    if !agent.still_current()? {
        return Err(Error::no_agent(name));
    }
    agent.write_hold(&HoldFile {
        key: key.into(),
        note: note.map(str::to_string),
        created_at: util::now_stamp(),
        hash: None,
        length: None,
        input_id: None,
    })?;
    let mut st = agent.state()?;
    agent.touch(&mut st, "hold", &format!("hold:{key}"))?;
    ops::refresh_display(&agent, &st);
    Ok(())
}

pub fn release(agent_ref: &AgentRef, key: Option<&str>, all: bool) -> Result<()> {
    let name = &agent_ref.to_string();
    let home = Home::resolve()?;
    let agent = Agent::require(&home, agent_ref)?;
    let _g = home.state_lock(agent_ref)?;
    let keys: Vec<String> = if all {
        agent.holds()?.into_iter().map(|h| h.key).collect()
    } else {
        let key = key.unwrap_or("manual");
        if !util::valid_name(key) {
            return Err(Error::usage(format!("invalid hold key {key:?}")));
        }
        vec![key.to_string()]
    };
    for k in &keys {
        if !agent.remove_hold(k)? {
            warn(format!("{name} has no hold {k:?}"));
        }
    }
    let mut st = agent.state()?;
    agent.touch(&mut st, "release", &format!("release:{}", keys.join(",")))?;
    ops::refresh_display(&agent, &st);
    Ok(())
}

// ---- status / list ----

#[derive(Serialize)]
struct TmuxOut {
    session: String,
    window: String,
    pane: Option<String>,
}

#[derive(Serialize)]
struct StatusOut {
    project: String,
    name: String,
    instance: String,
    driver: String,
    state: String,
    since: String,
    holds: Vec<String>,
    cwd: String,
    tmux: TmuxOut,
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    created_at: String,
}

fn status_of(agent: &Agent) -> Result<StatusOut> {
    let (st, pane) = ops::reconcile(agent)?;
    Ok(StatusOut {
        project: agent.project().into(),
        name: agent.name().into(),
        instance: agent.instance().into(),
        driver: agent.info.driver.clone(),
        state: st.state.as_str().into(),
        since: st.since.clone(),
        holds: agent.active_holds(&st)?,
        cwd: agent.info.cwd.clone(),
        tmux: TmuxOut {
            session: agent.info.tmux.session.clone(),
            window: agent.name().into(),
            pane: pane
                .as_ref()
                .map(|p| p.id.clone())
                .or_else(|| agent.info.tmux.pane.clone()),
        },
        pid: pane.as_ref().filter(|p| !p.dead).and_then(|p| p.pid),
        exit_status: st.exit_status,
        session_id: st.session_id.clone(),
        created_at: agent.info.created_at.clone(),
    })
}

pub fn status(agent_ref: &AgentRef, field: Option<&str>) -> Result<()> {
    let home = Home::resolve()?;
    let agent = Agent::require(&home, agent_ref)?;
    let out = status_of(&agent)?;
    let value = serde_json::to_value(&out).map_err(|e| Error::runtime(e.to_string()))?;
    match field {
        None => {
            let text = serde_yaml_ng::to_string(&out).map_err(|e| Error::runtime(e.to_string()))?;
            print!("{text}");
        }
        Some(f) => {
            let v = f
                .split('.')
                .try_fold(&value, |v, k| v.get(k))
                .ok_or_else(|| Error::usage(format!("unknown field {f:?}")))?;
            match v {
                serde_json::Value::String(s) => println!("{s}"),
                serde_json::Value::Null => println!(),
                other => println!("{other}"),
            }
        }
    }
    Ok(())
}

fn age(stamp: &str) -> String {
    let s = util::elapsed_secs(stamp) as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// `project`: `None` lists every project (`-a`).
pub fn list(quiet: bool, project: Option<&str>) -> Result<()> {
    let home = Home::resolve()?;
    let mut rows = Vec::new();
    for agent_ref in record::list(&home, project)? {
        if quiet {
            println!("{agent_ref}");
            continue;
        }
        let agent = match Agent::load(&home, &agent_ref) {
            Ok(Some(a)) => a,
            Ok(None) => continue,
            Err(e) => {
                warn(e);
                continue;
            }
        };
        match status_of(&agent) {
            Ok(s) => rows.push([
                format!("{}/{}", s.project, s.name),
                s.driver,
                s.state,
                age(&s.since),
                if s.holds.is_empty() {
                    "-".into()
                } else {
                    s.holds.join(",")
                },
                s.tmux.pane.unwrap_or_else(|| "-".into()),
                s.cwd,
            ]),
            Err(e) => warn(e),
        }
    }
    if quiet {
        return Ok(());
    }
    let header = ["NAME", "DRIVER", "STATE", "FOR", "HOLDS", "PANE", "CWD"].map(String::from);
    let mut widths = header.clone().map(|h| h.chars().count());
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    for r in std::iter::once(&header).chain(rows.iter()) {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            if i + 1 == r.len() {
                line.push_str(c);
            } else {
                line.push_str(&format!("{c:<w$}  ", w = widths[i]));
            }
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

// ---- kill ----

pub fn kill(agent_ref: &AgentRef, timeout: Option<f64>, force: bool) -> Result<()> {
    let name = &agent_ref.to_string();
    let home = Home::resolve()?;
    let cfg = config::load(&home)?;
    let agent = Agent::require(&home, agent_ref)?;
    let sig = util::signal_flag();
    let _input = home.lock(agent_ref, LockKind::Input, None, Some(&sig))?;
    if !agent.still_current()? {
        return Err(Error::NoAgent(format!(
            "agent {name} is gone or was replaced"
        )));
    }
    {
        let _g = home.state_lock(agent_ref)?;
        let st = agent.state()?;
        agent.expire_holds(&st)?;
        let holds = agent.active_holds(&st)?;
        if !holds.is_empty() && !force {
            return Err(Error::NotReady(format!(
                "{name} is held ({}); release it or use --force",
                holds.join(",")
            )));
        }
    }
    let instance = agent.instance().to_string();
    let tmux = Tmux::for_agent(&agent.info.tmux);
    let (st, pane) = ops::reconcile(&agent)?;
    let limit = Duration::from_secs_f64(timeout.unwrap_or(cfg.kill_timeout));
    // The process to make sure of later: pid and start time (§11 step 5).
    let mut tracked: Option<(u32, String)> = None;
    if let Some(p) = pane.as_ref().filter(|p| !p.dead) {
        if let Some(pid) = p.pid {
            tracked = util::process_start_time(pid).map(|start| (pid, start));
        }
        if !force {
            // The driver's way to end the CLI; without one (or if the driver
            // cannot be loaded any more), SIGTERM.
            let exit = match Driver::load(&agent.home, &agent.info.driver) {
                Ok(d) => d.exit_text().map(|t| (t.to_string(), d.submit_delay_ms())),
                Err(e) => {
                    warn(format!("{e}; ending {name} with SIGTERM"));
                    None
                }
            };
            if st.state == State::Idle
                && let Some((exit_text, delay)) = &exit
            {
                let buffer = format!("tmac-{instance}-exit");
                if tmux.load_buffer(&buffer, exit_text.as_bytes()).is_ok()
                    && tmux
                        .paste_tagged(&p.id, &instance, &buffer)
                        .unwrap_or(false)
                {
                    std::thread::sleep(Duration::from_millis(*delay));
                    let _ = tmux.send_key_tagged(&p.id, &instance, "Enter");
                } else {
                    tmux.delete_buffer(&buffer);
                }
            } else if let Some((pid, start)) = &tracked
                && util::same_process(*pid, start)
            {
                // SAFETY: the pid was checked against its start time.
                unsafe { libc::kill(*pid as i32, libc::SIGTERM) };
            }
            let deadline = Instant::now() + limit;
            while Instant::now() < deadline {
                match tmux.find_pane(&instance)? {
                    Some(p) if !p.dead => {}
                    _ => break,
                }
                util::sleep_checked(Duration::from_millis(200), &sig)?;
            }
        }
    }
    if let Some(p) = tmux.find_pane(&instance)? {
        tmux.kill_pane_tagged(&p.id, &instance)?;
        if let Some((pid, start)) = &tracked {
            // Give the CLI 2 seconds to go after its pane closed.
            let grace = Instant::now() + Duration::from_secs(2);
            while Instant::now() < grace && util::same_process(*pid, start) {
                std::thread::sleep(Duration::from_millis(100));
            }
            if util::same_process(*pid, start) {
                // SAFETY: the pid was checked against its start time.
                unsafe {
                    let pgid = libc::getpgid(*pid as i32);
                    if pgid == *pid as i32 {
                        libc::kill(-pgid, libc::SIGKILL);
                    } else {
                        libc::kill(*pid as i32, libc::SIGKILL);
                    }
                }
            }
        }
    }
    record::remove(&agent)
}
