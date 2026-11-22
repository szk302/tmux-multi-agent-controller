//! `tmac project`, `tmac session` and `tmac attach` (§3.1, §5.8, §10.1):
//! names and the terminal side of the tmux sessions agents run in.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config;
use crate::error::{Error, Result};
use crate::home::Home;
use crate::project::{self, AgentRef};
use crate::record::Agent;
use crate::tmux::{Tmux, script_line};

/// Variables `session --create` does not give the session: tmux's own, and
/// the ones that name an agent (set when it runs in an agent's pane).
const NOT_COPIED: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "TMAC_PROJECT",
    "TMAC_AGENT",
    "TMAC_INSTANCE",
];

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
            create_session(&tmux, &name, &cwd)?;
        }
    }
    println!("{name}");
    Ok(())
}

/// Create the session with this process's environment (§5.8). A server
/// that already runs gives a new session its own environment (only the
/// variables of `update-environment` come from the client), so the
/// differences are set on the session (`set-environment`, and `-r` for the
/// variables this process does not have), and the first window's shell is
/// started again with them. All of it is one line given on standard input:
/// the values are not in any process's arguments, and other clients see
/// the session only once it is ready.
fn create_session(tmux: &Tmux, name: &str, cwd: &Path) -> Result<()> {
    let target = format!("={name}");
    let cwd = cwd.to_string_lossy().into_owned();
    let mut commands: Vec<Vec<String>> = vec![vec![
        "new-session".into(),
        "-d".into(),
        "-s".into(),
        name.to_string(),
        "-c".into(),
        cwd.clone(),
    ]];
    // No server: the new one starts with this process's environment.
    if let Some(server) = tmux.environment(None)? {
        let own: BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .filter(|(k, _)| !NOT_COPIED.contains(&k.as_str()))
            .collect();
        for (k, v) in &own {
            if server.get(k) != Some(v) {
                commands.push(vec![
                    "set-environment".into(),
                    "-t".into(),
                    target.clone(),
                    k.clone(),
                    v.clone(),
                ]);
            }
        }
        for k in server.keys() {
            if !own.contains_key(k) && !NOT_COPIED.contains(&k.as_str()) {
                commands.push(vec![
                    "set-environment".into(),
                    "-t".into(),
                    target.clone(),
                    "-r".into(),
                    k.clone(),
                ]);
            }
        }
        if commands.len() > 1 {
            // The shell just started with the server's environment.
            commands.push(vec![
                "respawn-pane".into(),
                "-k".into(),
                "-c".into(),
                cwd,
                "-t".into(),
                format!("{target}:"),
            ]);
        }
    }
    match tmux.run_script(&script_line(&commands)) {
        Ok(_) => Ok(()),
        // Made by someone else in between: the rest of the line was skipped.
        Err(e) if e.to_string().contains("duplicate session") => Ok(()),
        Err(e) => Err(e),
    }
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
    // Decide whether this terminal can show it before selecting the window,
    // so that a refused attach changes nothing.
    match std::env::var("TMUX").ok().filter(|v| !v.is_empty()) {
        None => {
            // tmux attaches standard input, which must be a terminal.
            // SAFETY: isatty only looks at the descriptor.
            if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
                return Err(Error::runtime(format!(
                    "attach needs a terminal, and standard input is not one; run it in a \
                     terminal, or `tmux -S {socket} attach -t ={session}`"
                )));
            }
            tmux.select(&pane.id)?;
            Err(tmux.exec_attach(&session))
        }
        Some(v) => {
            let inside = v.split(',').next().unwrap_or("");
            if inside != socket {
                return Err(Error::runtime(format!(
                    "this terminal is inside another tmux server ({inside}); {name} runs on \
                     {socket}. Detach first, or run `tmux -S {socket} attach -t ={session}`"
                )));
            }
            tmux.select(&pane.id)?;
            tmux.run(&["switch-client".into(), "-t".into(), format!("={session}")])
                .map(|_| ())
        }
    }
}
