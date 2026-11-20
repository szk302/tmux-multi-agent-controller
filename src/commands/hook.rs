//! `tmac hook` (§6): called by the CLI's hooks.

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::{self, Config};
use crate::driver::{After, Before, Driver};
use crate::error::{Result, warn};
use crate::home::Home;
use crate::ops;
use crate::project::AgentRef;
use crate::record::{Agent, HUMAN, HoldFile, Progress, State, UNCONFIRMED};
use crate::util;

pub struct HookArgs {
    pub project: Option<String>,
    pub agent: Option<String>,
    pub instance: Option<String>,
    pub driver: String,
    pub event: String,
}

/// Run the hook. Never returns 2 by itself: only a relay may (§6.5).
pub fn run(args: HookArgs) -> i32 {
    let mut payload = Vec::new();
    let _ = io::stdin().read_to_end(&mut payload);
    let project = args
        .project
        .or_else(|| std::env::var("TMAC_PROJECT").ok())
        .filter(|s| !s.is_empty());
    let name = args
        .agent
        .or_else(|| std::env::var("TMAC_AGENT").ok())
        .filter(|s| !s.is_empty());
    let instance = args
        .instance
        .or_else(|| std::env::var("TMAC_INSTANCE").ok())
        .filter(|s| !s.is_empty());
    let (Some(project), Some(name), Some(instance)) = (project, name, instance) else {
        return 0;
    };
    let Some(driver) = Driver::from_name(&args.driver) else {
        eprintln!("tmac: hook: unknown driver {:?}", args.driver);
        return 1;
    };
    if !util::valid_name(&name) || !util::valid_name(&project) {
        eprintln!("tmac: hook: invalid agent {project}/{name}");
        return 1;
    }
    let key = AgentRef { project, name };
    let home = match Home::resolve() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("tmac: hook: {e}");
            return 1;
        }
    };
    let agent = match Agent::load(&home, &key) {
        Ok(Some(a)) if a.instance() == instance => a,
        Ok(_) => {
            warn(format!(
                "ignoring {} hook of an earlier {key} ({instance})",
                args.event
            ));
            return 0;
        }
        Err(e) => {
            eprintln!("tmac: hook: {e}");
            return 1;
        }
    };
    let cfg = config::load(&home).unwrap_or_else(|e| {
        warn(format!("{e}; using the default settings"));
        Config::default()
    });
    let json: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
    let event = args.event.as_str();

    let mut failed = false;
    let before = driver.before(event, &json);
    let logged = match apply_before(&agent, driver, before, event, &json) {
        Ok(Some(logged)) => logged,
        Ok(None) => {
            warn(format!("ignoring {event} hook: {key} was replaced"));
            return 0;
        }
        Err(e) => {
            warn(format!("{event}: cannot update the state of {key}: {e}"));
            failed = true;
            false
        }
    };

    let relay = agent
        .info
        .relay
        .get(event)
        .filter(|c| !c.trim().is_empty())
        .cloned();
    let (code, blocked) = match &relay {
        Some(cmd) => run_relay(driver, cmd, &payload, event, cfg.relay_timeout),
        None => (0, false),
    };

    let after = driver.after(event, &json);
    if let Err(e) = apply_after(&agent, after, event, blocked, logged) {
        warn(format!("{event}: cannot update the state of {key}: {e}"));
        failed = true;
    }
    if relay.is_some() {
        code
    } else if failed {
        1
    } else {
        0
    }
}

/// Changes before the relay. `Ok(None)` if the record was replaced;
/// otherwise whether the event was logged.
fn apply_before(
    agent: &Agent,
    driver: Driver,
    before: Before,
    event: &str,
    payload: &Value,
) -> Result<Option<bool>> {
    let _g = agent.home.state_lock(&agent.key())?;
    if !agent.still_current()? {
        return Ok(None);
    }
    let mut st = agent.state()?;
    agent.expire_holds(&st)?;
    let holds_before = agent.active_holds(&st)?;
    let from = st.state;
    let logged = match before {
        Before::None => false,
        Before::To(s) => {
            agent.transition(&mut st, s, "hook", event)?;
            true
        }
        Before::Stop => {
            st.stop_seq += 1;
            agent.touch(&mut st, "hook", event)?;
            true
        }
        Before::Prompt => {
            match_prompt(agent, driver, payload, st.state)?;
            agent.transition(&mut st, State::Busy, "hook", event)?;
            true
        }
    };
    if st.state != from || agent.active_holds(&st)? != holds_before {
        ops::refresh_display(agent, &st);
    }
    Ok(Some(logged))
}

/// Match a submitted prompt against tmac's input; otherwise treat it as a
/// person typing in the pane (§7.4, §9.3).
fn match_prompt(agent: &Agent, driver: Driver, payload: &Value, state: State) -> Result<()> {
    let prompt = payload.get("prompt").and_then(Value::as_str).unwrap_or("");
    let hash = util::text_hash(prompt);
    let mut ours = false;
    if let Some(hold) = agent.hold(UNCONFIRMED)? {
        // Whatever was in the input box has now been submitted.
        agent.remove_hold(UNCONFIRMED)?;
        ours |= hold.hash.as_deref() == Some(hash.as_str());
    }
    if let Some(mut input) = agent.input()?
        && input.progress == Progress::Pending
        && input.hash == hash
    {
        input.progress = Progress::Submitted;
        agent.write_input(&input)?;
        ours = true;
    }
    // A prompt during start-up comes from the command line (a first
    // prompt), not from a person typing.
    // Prompts the CLI makes itself (notices of background tasks) are not typed either.
    if !ours
        && state != State::Starting
        && !driver.generated_prompt(prompt)
        && agent.info.human_hold > 0.0
    {
        agent.write_hold(&HoldFile {
            key: HUMAN.into(),
            note: Some("input typed in the pane".into()),
            created_at: util::now_stamp(),
            hash: None,
            length: None,
            input_id: None,
        })?;
    }
    Ok(())
}

/// Changes after the relay.
fn apply_after(
    agent: &Agent,
    after: After,
    event: &str,
    blocked: bool,
    logged: bool,
) -> Result<()> {
    let _g = agent.home.state_lock(&agent.key())?;
    if !agent.still_current()? {
        return Ok(());
    }
    let mut st = agent.state()?;
    agent.expire_holds(&st)?;
    let holds_before = agent.active_holds(&st)?;
    let from = st.state;
    match after {
        After::None => {
            if !logged {
                agent.touch(&mut st, "hook", event)?;
            }
        }
        After::To(s) => {
            agent.transition(&mut st, s, "hook", event)?;
        }
        After::Prompt => {
            if let Some(mut input) = agent.input()?
                && input.progress == Progress::Submitted
            {
                input.progress = if blocked {
                    Progress::Rejected
                } else {
                    Progress::Confirmed
                };
                agent.write_input(&input)?;
            }
            if blocked {
                // The CLI drops a rejected prompt and stays ready for input.
                agent.transition(&mut st, State::Idle, "hook", &format!("{event}(rejected)"))?;
            }
        }
        After::Stop => {
            let to = if blocked { State::Busy } else { State::Idle };
            let label = if blocked {
                format!("{event}(continued)")
            } else {
                event.to_string()
            };
            agent.transition(&mut st, to, "hook", &label)?;
        }
    }
    if st.state != from || agent.active_holds(&st)? != holds_before {
        ops::refresh_display(agent, &st);
    }
    Ok(())
}

/// Run the relay command with the hook input (§6.3). Returns its exit code
/// and whether it blocked.
fn run_relay(driver: Driver, cmd: &str, payload: &[u8], event: &str, timeout: f64) -> (i32, bool) {
    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .env("TMAC_EVENT", event)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tmac: hook: cannot run the relay for {event}: {e}");
            return (1, false);
        }
    };
    let stdin = child.stdin.take();
    let data = payload.to_vec();
    let writer = thread::spawn(move || {
        if let Some(mut s) = stdin {
            let _ = s.write_all(&data);
        }
    });
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let pgid = child.id() as i32;
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            // SAFETY: plain signal to the relay's own process group.
            unsafe { libc::kill(-pgid, libc::SIGTERM) };
            let grace = Instant::now() + Duration::from_secs(2);
            loop {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                if Instant::now() >= grace {
                    // SAFETY: as above.
                    unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = writer.join();
            let _ = reader.join();
            eprintln!(
                "tmac: hook: the relay for {event} did not finish within {timeout}s and was stopped"
            );
            return (1, false);
        }
        thread::sleep(Duration::from_millis(20));
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let mut so = io::stdout();
    let _ = so.write_all(&out);
    let _ = so.flush();
    let code = status.and_then(|s| s.code()).unwrap_or(1);
    (code, driver.relay_blocked(code, &out))
}
