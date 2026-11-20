//! tmux operations. Every operation on a pane checks its tag
//! (`@tmac_instance`) in the same tmux invocation (§3.5, §7.3, §11).

use std::io::Write;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::record::TmuxInfo;

/// A tmux client bound to one server.
#[derive(Debug, Clone)]
pub struct Tmux {
    base: Vec<String>,
}

/// A pane found by its tag.
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub id: String,
    pub dead: bool,
    pub dead_status: Option<i64>,
    pub pid: Option<u32>,
}

const MISMATCH: &str = "tmac-mismatch";

fn no_server(stderr: &str) -> bool {
    stderr.contains("no server running")
        || stderr.contains("error connecting to")
        || stderr.contains("No such file or directory")
}

/// Escape an argument for tmux's argument list, where an unescaped trailing
/// `;` separates commands.
pub fn escape_arg(s: &str) -> String {
    if s.ends_with(';') && !s.ends_with("\\;") {
        format!("{}\\;", &s[..s.len() - 1])
    } else {
        s.to_string()
    }
}

/// Quote a value for a tmux command string (used inside `if-shell`).
fn tq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Tmux {
    /// Client for a socket name (`-L`) or the default server.
    pub fn with_socket_name(name: Option<&str>) -> Tmux {
        let base = match name {
            Some(n) => vec!["-L".into(), n.into()],
            None => Vec::new(),
        };
        Tmux { base }
    }

    /// Client for the server that runs an agent.
    pub fn for_agent(info: &TmuxInfo) -> Tmux {
        match (&info.socket, &info.socket_name) {
            (Some(path), _) => Tmux {
                base: vec!["-S".into(), path.clone()],
            },
            (None, name) => Tmux::with_socket_name(name.as_deref()),
        }
    }

    fn command(&self, args: &[String]) -> Command {
        let mut c = Command::new("tmux");
        c.args(&self.base).args(args);
        c
    }

    /// Run a tmux command; error on failure.
    pub fn run(&self, args: &[String]) -> Result<String> {
        let out = self
            .command(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::runtime(format!("cannot run tmux: {e}")))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(Error::runtime(format!(
                "tmux {} failed: {}",
                args.first().map(String::as_str).unwrap_or(""),
                err.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run a tmux command, returning (success, stdout, stderr).
    fn try_run(&self, args: &[String]) -> Result<(bool, String, String)> {
        let out = self
            .command(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::runtime(format!("cannot run tmux: {e}")))?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    pub fn has_session(&self, session: &str) -> Result<bool> {
        let (ok, _, _) =
            self.try_run(&["has-session".into(), "-t".into(), format!("={session}")])?;
        Ok(ok)
    }

    /// Find the pane tagged with `instance` on this server.
    pub fn find_pane(&self, instance: &str) -> Result<Option<PaneInfo>> {
        let fmt = "#{pane_id}\t#{@tmac_instance}\t#{pane_dead}\t#{pane_dead_status}\t#{pane_pid}";
        let (ok, out, err) =
            self.try_run(&["list-panes".into(), "-a".into(), "-F".into(), fmt.into()])?;
        if !ok {
            if no_server(&err) {
                return Ok(None);
            }
            return Err(Error::runtime(format!(
                "tmux list-panes failed: {}",
                err.trim()
            )));
        }
        for line in out.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() >= 5 && f[1] == instance {
                return Ok(Some(PaneInfo {
                    id: f[0].to_string(),
                    dead: f[2] == "1",
                    dead_status: f[3].parse().ok(),
                    pid: f[4].parse().ok(),
                }));
            }
        }
        Ok(None)
    }

    /// Run `cmd` (a tmux command string) only if `pane` still carries the
    /// tag `instance`. Returns whether it ran.
    pub fn if_tagged(&self, pane: &str, instance: &str, cmd: &str) -> Result<bool> {
        let cond = format!("#{{==:#{{@tmac_instance}},{instance}}}");
        let args = vec![
            "if-shell".into(),
            "-F".into(),
            "-t".into(),
            pane.into(),
            cond,
            cmd.into(),
            format!("display-message -p {MISMATCH}"),
        ];
        let (ok, out, err) = self.try_run(&args)?;
        if !ok {
            if no_server(&err)
                || err.contains("can't find pane")
                || err.contains("can't find window")
            {
                return Ok(false);
            }
            return Err(Error::runtime(format!(
                "tmux if-shell failed: {}",
                err.trim()
            )));
        }
        Ok(!out.contains(MISMATCH))
    }

    /// Load text into a named buffer through standard input (§7.3).
    pub fn load_buffer(&self, name: &str, data: &[u8]) -> Result<()> {
        let mut child = self
            .command(&["load-buffer".into(), "-b".into(), name.into(), "-".into()])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::runtime(format!("cannot run tmux: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(data)?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(Error::runtime(format!(
                "tmux load-buffer failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    pub fn delete_buffer(&self, name: &str) {
        let _ = self.try_run(&["delete-buffer".into(), "-b".into(), name.into()]);
    }

    /// Paste a named buffer with bracketed paste and delete it, if tagged.
    pub fn paste_tagged(&self, pane: &str, instance: &str, buffer: &str) -> Result<bool> {
        self.if_tagged(
            pane,
            instance,
            &format!("paste-buffer -p -d -b {} -t {}", tq(buffer), tq(pane)),
        )
    }

    /// Send a key (e.g. `Enter`), if tagged.
    pub fn send_key_tagged(&self, pane: &str, instance: &str, key: &str) -> Result<bool> {
        self.if_tagged(
            pane,
            instance,
            &format!("send-keys -t {} {}", tq(pane), tq(key)),
        )
    }

    /// Close the pane, if tagged.
    pub fn kill_pane_tagged(&self, pane: &str, instance: &str) -> Result<bool> {
        self.if_tagged(pane, instance, &format!("kill-pane -t {}", tq(pane)))
    }

    /// Update the display options `@tmac_state` and `@tmac_hold` (§4.4).
    pub fn set_display(
        &self,
        pane: &str,
        instance: &str,
        state: &str,
        holds: &[String],
    ) -> Result<bool> {
        let cmd = format!(
            "set-option -p -t {p} @tmac_state {s} ; set-option -p -t {p} @tmac_hold {h}",
            p = tq(pane),
            s = tq(state),
            h = tq(&holds.join(","))
        );
        self.if_tagged(pane, instance, &cmd)
    }

    /// Socket path of the server (`#{socket_path}`) as seen from a pane.
    pub fn socket_path(&self, pane: &str) -> Result<String> {
        Ok(self
            .run(&[
                "display-message".into(),
                "-p".into(),
                "-t".into(),
                pane.into(),
                "#{socket_path}".into(),
            ])?
            .trim()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping() {
        assert_eq!(escape_arg("a;"), "a\\;");
        assert_eq!(escape_arg(";"), "\\;");
        assert_eq!(escape_arg("a\\;"), "a\\;");
        assert_eq!(escape_arg("a;b"), "a;b");
        assert_eq!(tq("it's"), "'it'\\''s'");
    }
}
