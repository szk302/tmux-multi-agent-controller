//! Shared operations: reconciling with tmux (§3.5), deciding whether input
//! is possible (§1.4) and typing input (§7.3).

use std::sync::atomic::AtomicUsize;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::driver::Driver;
use crate::error::{Error, Result};
use crate::home::LockKind;
use crate::record::{Agent, HoldFile, InputFile, Progress, State, StateFile, UNCONFIRMED};
use crate::tmux::{PaneInfo, Tmux};
use crate::util;

/// Reflect a vanished or dead pane into the state (§3.5). Returns the
/// current state and the pane, if any.
pub fn reconcile(agent: &Agent) -> Result<(StateFile, Option<PaneInfo>)> {
    let tmux = Tmux::for_agent(&agent.info.tmux);
    let pane = tmux.find_pane(agent.instance())?;
    let st = agent.state()?;
    if st.state == State::Exited {
        return Ok((st, pane));
    }
    let exit_status = match &pane {
        Some(p) if !p.dead => return Ok((st, pane)),
        Some(p) => p.dead_status,
        None => {
            // spawn writes the record before it creates the pane.
            let grace = agent.info.spawn_timeout + 60.0;
            if st.state == State::Starting && util::elapsed_secs(&agent.info.created_at) < grace {
                return Ok((st, pane));
            }
            None
        }
    };
    let _g = agent.home.state_lock(&agent.key())?;
    if !agent.still_current()? {
        return Err(Error::NoAgent(format!(
            "agent {} was replaced",
            agent.label()
        )));
    }
    let mut st = agent.state()?;
    if st.state != State::Exited {
        st.exit_status = exit_status;
        let event = if pane.is_some() {
            "pane-dead"
        } else {
            "pane-gone"
        };
        agent.transition(&mut st, State::Exited, "check", event)?;
        refresh_display(agent, &st);
    }
    Ok((st, pane))
}

/// Whether tmac may type now: `idle` for at least `send.settle_sec`, and no
/// hold in effect (§1.4).
pub fn can_input(agent: &Agent, cfg: &Config, st: &StateFile) -> Result<bool> {
    Ok(st.state == State::Idle
        && util::elapsed_secs(&st.since) >= cfg.send_settle_sec
        && agent.active_holds(st)?.is_empty())
}

/// Update the display options of the pane (§4.4). Errors are ignored.
pub fn refresh_display(agent: &Agent, st: &StateFile) {
    let Some(pane) = agent.info.tmux.pane.as_deref() else {
        return;
    };
    let holds = agent.active_holds(st).unwrap_or_default();
    let tmux = Tmux::for_agent(&agent.info.tmux);
    let _ = tmux.set_display(pane, agent.instance(), st.state.as_str(), &holds);
}

/// Re-read the state under the state lock and refresh the display.
pub fn refresh_display_now(agent: &Agent) {
    if let Ok(st) = agent.state() {
        refresh_display(agent, &st);
    }
}

/// Move an unfinished input to the `unconfirmed` hold (caller holds the
/// state lock). Returns the final progress if it was in fact decided.
fn settle_input(agent: &Agent) -> Result<Option<Progress>> {
    let Some(input) = agent.input()? else {
        return Ok(None);
    };
    match input.progress {
        Progress::Confirmed | Progress::Rejected => {
            agent.remove_input()?;
            Ok(Some(input.progress))
        }
        Progress::Pending | Progress::Submitted => {
            agent.write_hold(&HoldFile {
                key: UNCONFIRMED.into(),
                note: Some("tmac could not confirm that its input was submitted".into()),
                created_at: util::now_stamp(),
                hash: Some(input.hash.clone()),
                length: Some(input.length),
                input_id: Some(input.id.clone()),
            })?;
            agent.remove_input()?;
            Ok(None)
        }
    }
}

/// Result of one attempt to type.
pub enum Attempt {
    /// The agent could not take input after all; wait and retry.
    NotReady,
    /// The input was submitted and confirmed.
    Done,
}

/// Type `text` into the agent and wait for confirmation (§7.3 steps 2-5).
/// `lock_wait` bounds the wait for the input lock (`None`: no limit).
pub fn attempt_input(
    agent: &Agent,
    cfg: &Config,
    text: &str,
    lock_wait: Option<Duration>,
    sig: &AtomicUsize,
) -> Result<Attempt> {
    let key = agent.key();
    let name = key.to_string();
    let instance = agent.instance();
    let _input_lock = agent
        .home
        .lock(&key, LockKind::Input, lock_wait, Some(sig))?;
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    {
        let _g = agent.home.state_lock(&key)?;
        if !agent.still_current()? {
            return Err(Error::NoAgent(format!("agent {name} was replaced")));
        }
        if agent.input()?.is_some() && settle_input(agent)?.is_none() {
            // A previous input stopped half way: hold further input (§7.3).
            refresh_display_now(agent);
            return Ok(Attempt::NotReady);
        }
        let st = agent.state()?;
        if st.state == State::Exited {
            return Err(Error::NoAgent(format!("agent {name} has exited")));
        }
        agent.expire_holds(&st)?;
        if !can_input(agent, cfg, &st)? {
            return Ok(Attempt::NotReady);
        }
        agent.write_input(&InputFile {
            id: id.clone(),
            hash: util::text_hash(text),
            length: text.len(),
            at: util::now_stamp(),
            progress: Progress::Pending,
        })?;
    }
    let abandon = |agent: &Agent| {
        if let Ok(_g) = agent.home.state_lock(&key) {
            let _ = agent.remove_input();
        }
    };
    let tmux = Tmux::for_agent(&agent.info.tmux);
    let pane = match tmux.find_pane(instance) {
        Ok(Some(p)) if !p.dead => p,
        Ok(_) => {
            abandon(agent);
            return Err(Error::NoAgent(format!("the pane of {name} is gone")));
        }
        Err(e) => {
            abandon(agent);
            return Err(e);
        }
    };
    let buffer = format!("tmac-{instance}-{id}");
    let pasted = tmux
        .load_buffer(&buffer, text.as_bytes())
        .and_then(|()| tmux.paste_tagged(&pane.id, instance, &buffer));
    match pasted {
        Ok(true) => {}
        Ok(false) => {
            tmux.delete_buffer(&buffer);
            abandon(agent);
            return Err(Error::NoAgent(format!("the pane of {name} is gone")));
        }
        Err(e) => {
            tmux.delete_buffer(&buffer);
            abandon(agent);
            return Err(e);
        }
    }
    // From paste to Enter, signals do not interrupt (the flag is only read
    // afterwards), so the text is never left alone in the input box.
    let driver = Driver::from_name(&agent.info.driver).unwrap_or(Driver::Claude);
    let delay = cfg.submit_delay(driver.name(), driver.submit_delay_default_ms());
    thread::sleep(Duration::from_millis(delay));
    let entered = tmux.send_key_tagged(&pane.id, instance, "Enter");
    if !matches!(entered, Ok(true)) {
        let _g = agent.home.state_lock(&key)?;
        settle_input(agent)?;
        refresh_display_now(agent);
        return Err(Error::Unconfirmed(format!(
            "could not press Enter in the pane of {name}; input held as unconfirmed"
        )));
    }
    let deadline = Instant::now() + Duration::from_secs_f64(cfg.send_confirm_timeout);
    loop {
        let progress = {
            let _g = agent.home.state_lock(&key)?;
            agent.input()?.filter(|i| i.id == id).map(|i| i.progress)
        };
        match progress {
            Some(Progress::Confirmed) => {
                let _g = agent.home.state_lock(&key)?;
                agent.remove_input()?;
                return Ok(Attempt::Done);
            }
            Some(Progress::Rejected) => {
                let _g = agent.home.state_lock(&key)?;
                agent.remove_input()?;
                return Err(Error::Rejected(format!(
                    "{name} rejected the input (relay)"
                )));
            }
            None => {
                if !agent.still_current()? {
                    return Err(Error::NoAgent(format!("agent {name} is gone")));
                }
                return Err(Error::Unconfirmed(format!(
                    "the input record of {name} disappeared"
                )));
            }
            Some(_) => {}
        }
        let signal = util::check_signal(sig).err();
        if signal.is_some() || Instant::now() >= deadline {
            let _g = agent.home.state_lock(&key)?;
            match settle_input(agent)? {
                Some(Progress::Confirmed) => return Ok(Attempt::Done),
                Some(Progress::Rejected) => {
                    return Err(Error::Rejected(format!(
                        "{name} rejected the input (relay)"
                    )));
                }
                _ => {}
            }
            refresh_display_now(agent);
            if let Some(e) = signal {
                return Err(e);
            }
            return Err(Error::Unconfirmed(format!(
                "could not confirm the input to {name} within {}s; further input is held until it is \
                 confirmed or `tmac release {name} --key unconfirmed`",
                cfg.send_confirm_timeout
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Validate text to type (§7.2). Line endings are normalized to LF and
/// trailing line breaks removed.
pub fn prepare_text(raw: &str, max_bytes: usize) -> Result<String> {
    let text = raw.replace("\r\n", "\n");
    let text = text.trim_end_matches('\n').to_string();
    if text.is_empty() {
        return Err(Error::usage("the text is empty"));
    }
    if let Some(c) = text
        .chars()
        .find(|&c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(Error::usage(format!(
            "the text contains a control character U+{:04X}",
            c as u32
        )));
    }
    if text.len() > max_bytes {
        return Err(Error::usage(format!(
            "the text is {} bytes, more than send.max_bytes ({max_bytes})",
            text.len()
        )));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_rules() {
        assert_eq!(prepare_text("a\r\nb\n\n", 100).unwrap(), "a\nb");
        assert!(prepare_text("\n", 100).is_err());
        assert!(prepare_text("a\x1b[201~", 100).is_err());
        assert!(prepare_text("a\rb", 100).is_err());
        assert!(prepare_text("abcd", 3).is_err());
        assert_eq!(prepare_text("日本語\tです", 100).unwrap(), "日本語\tです");
    }
}
