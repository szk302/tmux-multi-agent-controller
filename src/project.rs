//! Projects (§3.1): agents are named within a project. Like docker compose,
//! the project defaults to the repository of the directory.

use std::fmt;
use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};
use crate::util;

/// An agent: its project and its name within the project.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AgentRef {
    pub project: String,
    pub name: String,
}

impl fmt::Display for AgentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.project, self.name)
    }
}

fn check(kind: &str, s: &str) -> Result<()> {
    if util::valid_name(s) {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "invalid {kind} {s:?}: use 1-64 of a-z 0-9 . _ - starting with a-z or 0-9"
        )))
    }
}

impl AgentRef {
    /// Parse `[PROJECT/]NAME`. Without a project, `project` gives it.
    pub fn parse(arg: &str, project: impl FnOnce() -> Result<String>) -> Result<AgentRef> {
        let (p, n) = match arg.split_once('/') {
            Some((p, n)) => {
                check("project", p)?;
                (p.to_string(), n)
            }
            None => (project()?, arg),
        };
        check("agent name", n)?;
        Ok(AgentRef {
            project: p,
            name: n.to_string(),
        })
    }
}

/// Turn a directory name into a project name: lower case, other characters
/// than `a-z 0-9 . _ -` replaced with `-`, starting with `a-z 0-9`, at most
/// 64 characters.
pub fn normalize(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_start_matches(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()));
    let s: String = s.chars().take(64).collect();
    if s.is_empty() { "default".into() } else { s }
}

/// The repository name of a directory. In a linked worktree it is the
/// main repository's directory; outside git, the directory itself.
pub fn derive(dir: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output();
    let base = match out {
        Ok(o) if o.status.success() => {
            let common = std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
            if common.file_name().is_some_and(|n| n == ".git") {
                common
                    .parent()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
            } else {
                // A bare repository: `name.git`.
                common.file_name().map(|n| {
                    let n = n.to_string_lossy();
                    n.strip_suffix(".git").unwrap_or(&n).to_string()
                })
            }
        }
        _ => None,
    };
    let base = base.or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()));
    normalize(&base.unwrap_or_default())
}

/// The project: `-p`, then `$TMAC_PROJECT`, then the repository of `dir`.
pub fn resolve(explicit: Option<&str>, dir: &Path) -> Result<String> {
    if let Some(p) = explicit.filter(|p| !p.is_empty()) {
        check("project", p)?;
        return Ok(p.to_string());
    }
    if let Ok(p) = std::env::var("TMAC_PROJECT")
        && !p.is_empty()
    {
        check("project (TMAC_PROJECT)", &p)?;
        return Ok(p);
    }
    Ok(derive(dir))
}

/// Resolve an `[PROJECT/]NAME` argument against the current directory.
pub fn agent_ref(arg: &str, explicit: Option<&str>) -> Result<AgentRef> {
    AgentRef::parse(arg, || resolve(explicit, &std::env::current_dir()?))
}

/// tmux session name for a project (tmux does not allow `.` or `:`).
pub fn session_name(project: &str) -> String {
    project.replace(['.', ':'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing() {
        assert_eq!(normalize("Sample-Todo"), "sample-todo");
        assert_eq!(normalize("my repo (copy)"), "my-repo--copy-");
        assert_eq!(normalize("_x"), "x");
        assert_eq!(normalize("日本語"), "default");
        assert_eq!(normalize(&"a".repeat(80)).len(), 64);
    }

    #[test]
    fn parsing() {
        let p = || Ok("here".to_string());
        assert_eq!(
            AgentRef::parse("coder", p).unwrap().to_string(),
            "here/coder"
        );
        assert_eq!(
            AgentRef::parse("other/coder", p).unwrap().to_string(),
            "other/coder"
        );
        assert!(AgentRef::parse("Bad/coder", p).is_err());
        assert!(AgentRef::parse("a/b/c", p).is_err());
        assert!(AgentRef::parse("", p).is_err());
    }

    #[test]
    fn derived_from_the_main_worktree() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("Main.Repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .output()
                    .unwrap()
                    .status
                    .success()
            )
        };
        git(&["init", "-q", "-b", "main"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ]);
        git(&["worktree", "add", "-q", ".worktree/feat", "-b", "feat"]);
        assert_eq!(derive(&repo), "main.repo");
        assert_eq!(derive(&repo.join(".worktree/feat")), "main.repo");
        let plain = t.path().join("Plain Dir");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(derive(&plain), "plain-dir");
        assert_eq!(session_name("main.repo"), "main-repo");
    }
}
