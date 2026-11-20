//! Errors and exit codes (§14.4).

use std::fmt;

/// An error that ends a command with a specific exit code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Runtime error (tmux, I/O, start-up failure): exit code 1.
    #[error("{0}")]
    Runtime(String),
    /// Usage or configuration error: exit code 2.
    #[error("{0}")]
    Usage(String),
    /// Timed out: exit code 3.
    #[error("{0}")]
    Timeout(String),
    /// No such agent, or it exited / was replaced: exit code 4.
    #[error("{0}")]
    NoAgent(String),
    /// Cannot take input now, or held: exit code 5.
    #[error("{0}")]
    NotReady(String),
    /// Input could not be confirmed: exit code 6.
    #[error("{0}")]
    Unconfirmed(String),
    /// Input was rejected by a relay: exit code 7.
    #[error("{0}")]
    Rejected(String),
    /// Interrupted by a signal: exit code 128 + signal number.
    #[error("interrupted by signal {0}")]
    Signal(i32),
}

impl Error {
    pub fn code(&self) -> i32 {
        match self {
            Error::Runtime(_) => 1,
            Error::Usage(_) => 2,
            Error::Timeout(_) => 3,
            Error::NoAgent(_) => 4,
            Error::NotReady(_) => 5,
            Error::Unconfirmed(_) => 6,
            Error::Rejected(_) => 7,
            Error::Signal(s) => 128 + s,
        }
    }

    pub fn runtime(msg: impl fmt::Display) -> Self {
        Error::Runtime(msg.to_string())
    }

    pub fn usage(msg: impl fmt::Display) -> Self {
        Error::Usage(msg.to_string())
    }

    pub fn no_agent(name: &str) -> Self {
        Error::NoAgent(format!("no such agent: {name}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Runtime(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Print a warning to standard error.
pub fn warn(msg: impl fmt::Display) {
    eprintln!("tmac: warning: {msg}");
}
