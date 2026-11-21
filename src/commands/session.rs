//! `tmac project`, `tmac session` and `tmac attach` (§3.1, §5.8, §10.1):
//! names and the terminal side of the tmux sessions agents run in.

use std::path::Path;

use crate::config;
use crate::error::{Error, Result};
use crate::home::Home;
use crate::project::{self, AgentRef};
use crate::record::Agent;
use crate::tmux::Tmux;

/// Print the project of a directory (§3.1). With `dir`, the project comes
/// from that directory only (rule 3), not from -p or TMAC_PROJECT.
pub fn project(explicit: Option<&str>, dir: Option<&Path>) -> Result<()> {
    let name = match dir {
        Some(d) => {
            let d = d
                .canonicalize()
                .ok()
                .filter(|p| p.is_dir())
                .ok_or_else(|| Error::usage(format!("not a directory: {}", d.display())))?;
            project::derive(&d)
        }
        None => project::resolve(explicit, &std::env::current_dir()?)?,
    };
    println!("{name}");
    Ok(())
}

/// Print the session the project's agents start in (§5.8); with `create`,
/// create it first if it does not exist, with this process's environment
/// (run it from the terminal so that agents get the user's environment).
pub fn session(explicit: Option<&str>, create: bool) -> Result<()> {
    let home = Home::resolve()?;
    let cfg = config::load(&home)?;
    let cwd = std::env::current_dir()?;
    let project = project::resolve(explicit, &cwd)?;
    let name = project::session_for(&cfg, &project)?;
    if create {
        let tmux = Tmux::with_socket_name(cfg.tmux_socket_name.as_deref());
        if !tmux.has_session(&name)? {
            let made = tmux.run(&[
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                name.clone(),
                "-c".into(),
                cwd.to_string_lossy().into_owned(),
            ]);
            match made {
                Ok(_) => {}
                // Made by someone else in between.
                Err(e) if e.to_string().contains("duplicate session") => {}
                Err(e) => return Err(e),
            }
        }
    }
    println!("{name}");
    Ok(())
}

/// Show the agent's window in this terminal (§10.1): select it, then
/// attach (or switch to its session inside tmux on the same server).
pub fn attach(agent_ref: &AgentRef) -> Result<()> {
    let name = agent_ref.to_string();
    let home = Home::resolve()?;
    let agent = Agent::require(&home, agent_ref)?;
    let tmux = Tmux::for_agent(&agent.info.tmux);
    let pane = tmux
        .find_pane(agent.instance())?
        .ok_or_else(|| Error::NoAgent(format!("the pane of {name} is gone")))?;
    let (session, socket) = tmux.pane_session(&pane.id)?;
    tmux.select(&pane.id)?;
    match std::env::var("TMUX").ok().filter(|v| !v.is_empty()) {
        None => Err(tmux.exec_attach(&session)),
        Some(v) => {
            let inside = v.split(',').next().unwrap_or("");
            if inside != socket {
                return Err(Error::runtime(format!(
                    "this terminal is inside another tmux server ({inside}); {name} runs on \
                     {socket}. Detach first, or run `tmux -S {socket} attach -t ={session}`"
                )));
            }
            tmux.run(&["switch-client".into(), "-t".into(), format!("={session}")])
                .map(|_| ())
        }
    }
}
