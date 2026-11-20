//! `tmac send` and `tmac wake` (§7).

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config;
use crate::error::{Error, Result};
use crate::home::Home;
use crate::ops::{self, Attempt};
use crate::project;
use crate::record::{Agent, State};
use crate::util;

pub struct SendArgs {
    pub project: Option<String>,
    pub name: String,
    pub text: Option<String>,
    pub file: Option<PathBuf>,
    pub timeout: Option<f64>,
    pub no_wait: bool,
}

pub struct WakeArgs {
    pub project: Option<String>,
    pub name: String,
    pub text: Option<String>,
    pub timeout: Option<f64>,
}

fn read_text(text: Option<String>, file: Option<PathBuf>) -> Result<String> {
    match (text, file) {
        (Some(_), Some(_)) => Err(Error::usage("give either TEXT or --file, not both")),
        (Some(t), None) => Ok(t),
        (None, Some(f)) => std::fs::read(&f)
            .map_err(|e| Error::usage(format!("cannot read {}: {e}", f.display())))
            .and_then(|b| String::from_utf8(b).map_err(|_| Error::usage("the text is not UTF-8"))),
        (None, None) => {
            let mut b = Vec::new();
            std::io::stdin().read_to_end(&mut b)?;
            String::from_utf8(b).map_err(|_| Error::usage("the text is not UTF-8"))
        }
    }
}

/// Remaining time to a deadline, for lock waits.
fn remaining(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|d| d.saturating_duration_since(Instant::now()))
}

/// Current agent, or exit code 4 if it is gone, replaced or exited.
fn live(agent: &Agent) -> Result<crate::record::StateFile> {
    if !agent.still_current()? {
        return Err(Error::NoAgent(format!(
            "agent {} is gone or was replaced",
            agent.name()
        )));
    }
    let (st, _) = ops::reconcile(agent)?;
    if st.state == State::Exited {
        return Err(Error::NoAgent(format!("agent {} has exited", agent.name())));
    }
    Ok(st)
}

pub fn send(a: SendArgs) -> Result<()> {
    let r = project::agent_ref(&a.name, a.project.as_deref())?;
    let name = r.to_string();
    let home = Home::resolve()?;
    let cfg = config::load(&home)?;
    let text = ops::prepare_text(&read_text(a.text, a.file)?, cfg.send_max_bytes)?;
    let agent = Agent::require(&home, &r)?;
    let sig = util::signal_flag();
    let deadline = a
        .timeout
        .map(|t| Instant::now() + Duration::from_secs_f64(t));
    loop {
        util::check_signal(&sig)?;
        let st = live(&agent)?;
        if ops::can_input(&agent, &cfg, &st)? {
            let lock_wait = if a.no_wait {
                Some(Duration::ZERO)
            } else {
                remaining(deadline)
            };
            match ops::attempt_input(&agent, &cfg, &text, lock_wait, &sig) {
                Ok(Attempt::Done) => return Ok(()),
                Ok(Attempt::NotReady) => {}
                Err(Error::Timeout(_)) if a.no_wait => {
                    return Err(Error::NotReady(format!("{} is taking other input", name)));
                }
                Err(e) => return Err(e),
            }
        }
        if a.no_wait {
            return Err(Error::NotReady(format!("{} cannot take input now", name)));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Error::Timeout(format!(
                "{} did not become ready for input in time",
                name
            )));
        }
        util::sleep_checked(Duration::from_millis(500), &sig)?;
    }
}

pub fn wake(a: WakeArgs) -> Result<()> {
    let r = project::agent_ref(&a.name, a.project.as_deref())?;
    let name = r.to_string();
    let home = Home::resolve()?;
    let cfg = config::load(&home)?;
    let text = ops::prepare_text(&read_text(a.text, None)?, cfg.send_max_bytes)?;
    let agent = Agent::require(&home, &r)?;
    let sig = util::signal_flag();
    let start_seq = agent.state()?.stop_seq;
    let deadline = a
        .timeout
        .map(|t| Instant::now() + Duration::from_secs_f64(t));
    loop {
        util::check_signal(&sig)?;
        let st = live(&agent)?;
        // A Stop that began after this command started will see the news.
        if st.stop_seq > start_seq {
            return Ok(());
        }
        if ops::can_input(&agent, &cfg, &st)? {
            match ops::attempt_input(&agent, &cfg, &text, remaining(deadline), &sig)? {
                Attempt::Done => return Ok(()),
                Attempt::NotReady => {}
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Error::Timeout(format!(
                "{} did not stop or become ready in time",
                name
            )));
        }
        util::sleep_checked(Duration::from_millis(500), &sig)?;
    }
}
