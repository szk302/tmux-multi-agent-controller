//! tmac: run hook-capable AI CLIs in tmux panes and type into them only when
//! they can take input. See `docs/prd.md` for the specification.

pub mod cli;
pub mod commands;
pub mod config;
pub mod driver;
pub mod error;
pub mod home;
pub mod hookconf;
pub mod ops;
pub mod project;
pub mod record;
pub mod tmux;
pub mod util;

use clap::Parser;

use crate::cli::{Cli, Cmd, DriverCmd, HooksCmd};
use crate::commands::{drivers, hook, hooks, input, manage, spawn};

/// Run the command line and return the exit code.
pub fn run() -> i32 {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(c) => c,
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                let _ = e.print();
                return 0;
            }
            let _ = e.print();
            // A hook must never return 2 by itself (§6.5).
            return if args.get(1).is_some_and(|a| a == "hook") {
                1
            } else {
                2
            };
        }
    };
    let project = cli.project;
    let pr = project.as_deref();
    let target = |name: &str| project::agent_ref(name, pr);
    let result = match cli.cmd {
        Cmd::Hook(h) => {
            return hook::run(hook::HookArgs {
                project,
                agent: h.agent,
                instance: h.instance,
                driver: h.driver,
                event: h.event,
            });
        }
        Cmd::Spawn(s) => spawn::run(spawn::SpawnArgs {
            project,
            name: s.name,
            driver: s.driver,
            cwd: s.cwd,
            env: s.env,
            pass_env: s.pass_env,
            relay: s.relay,
            session: s.session,
            human_hold: s.human_hold,
            allow_foreign_hooks: s.allow_foreign_hooks,
            timeout: s.timeout,
            command: s.command,
        }),
        Cmd::Send(s) => input::send(input::SendArgs {
            project,
            name: s.name,
            text: s.text,
            file: s.file,
            timeout: s.timeout,
            no_wait: s.no_wait,
        }),
        Cmd::Wake(w) => input::wake(input::WakeArgs {
            project,
            name: w.name,
            text: w.text,
            timeout: w.timeout,
        }),
        Cmd::Wait(w) => target(&w.name).and_then(|r| manage::wait(&r, &w.state, w.timeout)),
        Cmd::Hold(h) => target(&h.name)
            .and_then(|r| manage::hold(&r, h.key.as_deref(), h.note.as_deref(), h.timeout)),
        Cmd::Release(r) => {
            target(&r.name).and_then(|a| manage::release(&a, r.key.as_deref(), r.all))
        }
        Cmd::Status(s) => target(&s.name).and_then(|r| manage::status(&r, s.field.as_deref())),
        Cmd::List(l) => {
            if l.all {
                manage::list(l.quiet, None)
            } else {
                std::env::current_dir()
                    .map_err(error::Error::from)
                    .and_then(|d| project::resolve(pr, &d))
                    .and_then(|p| manage::list(l.quiet, Some(&p)))
            }
        }
        Cmd::Kill(k) => target(&k.name).and_then(|r| manage::kill(&r, k.timeout, k.force)),
        Cmd::Doctor(d) => match hooks::doctor(&d.drivers, d.cwd.as_deref()) {
            Ok(true) => Ok(()),
            Ok(false) => return 1,
            Err(e) => Err(e),
        },
        Cmd::Hooks(HooksCmd::Rulesync(r)) => hooks::rulesync(&r.dir, r.merge.as_deref(), &r.target),
        Cmd::Driver(DriverCmd::List) => drivers::list(),
        Cmd::Driver(DriverCmd::Show { name }) => drivers::show(&name),
        Cmd::Driver(DriverCmd::Check { target }) => drivers::check(target.as_deref()),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("tmac: {e}");
            e.code()
        }
    }
}
