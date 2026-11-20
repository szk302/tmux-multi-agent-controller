//! tmac: run hook-capable AI CLIs in tmux panes and type into them only when
//! they can take input. See `docs/prd.md` for the specification.

pub mod cli;
pub mod commands;
pub mod config;
pub mod driver;
pub mod error;
pub mod home;
pub mod ops;
pub mod record;
pub mod tmux;
pub mod util;

use clap::Parser;

use crate::cli::{Cli, Cmd};
use crate::commands::{hook, input, manage, spawn};

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
    let result = match cli.cmd {
        Cmd::Hook(h) => {
            return hook::run(hook::HookArgs {
                agent: h.agent,
                instance: h.instance,
                driver: h.driver,
                event: h.event,
            });
        }
        Cmd::Spawn(s) => spawn::run(spawn::SpawnArgs {
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
            name: s.name,
            text: s.text,
            file: s.file,
            timeout: s.timeout,
            no_wait: s.no_wait,
        }),
        Cmd::Wake(w) => input::wake(input::WakeArgs {
            name: w.name,
            text: w.text,
            timeout: w.timeout,
        }),
        Cmd::Wait(w) => manage::wait(&w.name, &w.state, w.timeout),
        Cmd::Hold(h) => manage::hold(&h.name, h.key.as_deref(), h.note.as_deref(), h.timeout),
        Cmd::Release(r) => manage::release(&r.name, r.key.as_deref(), r.all),
        Cmd::Status(s) => manage::status(&s.name, s.field.as_deref()),
        Cmd::List(l) => manage::list(l.quiet),
        Cmd::Kill(k) => manage::kill(&k.name, k.timeout, k.force),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("tmac: {e}");
            e.code()
        }
    }
}
