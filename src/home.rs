//! The data directory (§4.1, §4.2) and its locks (§4.3).

use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::project::AgentRef;
use crate::util;

/// The resolved data directory.
#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
}

/// The two per-agent locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    /// Serializes input and the operations that stop input.
    Input,
    /// Guards reads and writes of `state.yml`, `holds/` and `input.yml`.
    State,
}

/// A held lock; dropping it releases the lock.
#[derive(Debug)]
pub struct LockGuard {
    _file: File,
}

impl Home {
    /// `$TMAC_HOME`, or `~/.tmac`, as an absolute path.
    pub fn resolve() -> Result<Home> {
        let root = match env::var_os("TMAC_HOME").filter(|v| !v.is_empty()) {
            Some(v) => PathBuf::from(v),
            None => {
                let home = env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::usage("HOME is not set; set TMAC_HOME"))?;
                PathBuf::from(home).join(".tmac")
            }
        };
        let root = if root.is_absolute() {
            root
        } else {
            env::current_dir()?.join(root)
        };
        Ok(Home { root })
    }

    pub fn agents(&self) -> PathBuf {
        self.root.join("agents")
    }

    pub fn agent_dir(&self, agent: &AgentRef) -> PathBuf {
        self.agents().join(&agent.project).join(&agent.name)
    }

    pub fn creating(&self) -> PathBuf {
        self.agents().join(".creating")
    }

    pub fn removing(&self) -> PathBuf {
        self.agents().join(".removing")
    }

    pub fn lock_path(&self, agent: &AgentRef, kind: LockKind) -> PathBuf {
        let suffix = match kind {
            LockKind::Input => "input",
            LockKind::State => "state",
        };
        self.root
            .join("locks")
            .join(&agent.project)
            .join(format!("{}.{suffix}.lock", agent.name))
    }

    /// Acquire a lock. `timeout` of `None` waits without a limit; a signal
    /// recorded in `stop` ends the wait.
    pub fn lock(
        &self,
        agent: &AgentRef,
        kind: LockKind,
        timeout: Option<Duration>,
        stop: Option<&AtomicUsize>,
    ) -> Result<LockGuard> {
        lock_path_with(&self.lock_path(agent, kind), timeout, stop)
    }

    /// Acquire the state lock (held only briefly by everyone).
    pub fn state_lock(&self, agent: &AgentRef) -> Result<LockGuard> {
        self.lock(agent, LockKind::State, Some(Duration::from_secs(30)), None)
    }
}

/// Acquire an exclusive `flock(2)` on `path`.
pub fn lock_path_with(
    path: &Path,
    timeout: Option<Duration>,
    stop: Option<&AtomicUsize>,
) -> Result<LockGuard> {
    let file = util::open_lock_file(path)?;
    let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(LockGuard { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(Error::runtime(format!(
                    "cannot lock {}: {e}",
                    path.display()
                )));
            }
        }
        if let Some(stop) = stop {
            util::check_signal(stop)?;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Error::Timeout(format!(
                "timed out waiting for the lock {}",
                path.display()
            )));
        }
        thread::sleep(Duration::from_millis(20));
    }
}
