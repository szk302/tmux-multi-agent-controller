//! tmux operations. Every operation on a pane checks its tag
//! (`@tmac_instance`) in the same tmux invocation (§3.5, §7.3, §11).

use std::collections::BTreeMap;
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

/// Quote a value for a tmux command string (used inside `if-shell`).
fn tq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quote a word for a tmux command file: single quotes keep everything
/// as it is (`$`, `~`, `#{}`, `;`, newlines); a quote is closed, given in
/// double quotes and reopened.
fn script_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// One line of a tmux command file: the commands separated by ` ; `. tmux
/// runs a line as one group: if a command fails, the rest of the line is
/// skipped (tmux 3.7c).
pub fn script_line(commands: &[Vec<String>]) -> String {
    commands
        .iter()
        .map(|c| {
            c.iter()
                .map(|w| script_word(w))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" ; ")
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
        // A server that this client starts takes its environment as the
        // global one: keep out the variables that name an agent, set when
        // tmac runs inside an agent's pane (§5.4, §5.8).
        for k in ["TMAC_PROJECT", "TMAC_AGENT", "TMAC_INSTANCE"] {
            c.env_remove(k);
        }
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

    /// Run tmux commands given on standard input (`start-server ;
    /// source-file -`): the values in them do not appear in the arguments
    /// of a process, which other users may see (§5.3, §5.8), and their size
    /// is not limited like a command line (about 16 KB). Starts the server
    /// if none runs. Returns stdout.
    pub fn run_script(&self, line: &str) -> Result<String> {
        let mut child = self
            .command(&[
                "start-server".into(),
                ";".into(),
                "source-file".into(),
                "-".into(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::runtime(format!("cannot run tmux: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            let mut text = line.as_bytes().to_vec();
            text.push(b'\n');
            stdin.write_all(&text)?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(Error::runtime(format!(
                "tmux failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
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

    /// Seconds since the last key press of a terminal (tmux client) that
    /// shows the window of `pane` (§9.4). tmux updates `client_activity`
    /// when a terminal attaches and when a person presses a key, not for
    /// `send-keys` or `paste-buffer`. `None`: no terminal shows the window,
    /// or the pane or the server is gone.
    pub fn presence(&self, pane: &str) -> Result<Option<f64>> {
        let args: Vec<String> = [
            "display-message",
            "-p",
            "-t",
            pane,
            "#{window_id}",
            ";",
            "list-clients",
            "-F",
            "#{window_id}\t#{client_activity}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (ok, out, err) = self.try_run(&args)?;
        if !ok {
            if no_server(&err)
                || err.contains("can't find pane")
                || err.contains("can't find window")
            {
                return Ok(None);
            }
            return Err(Error::runtime(format!(
                "tmux list-clients failed: {}",
                err.trim()
            )));
        }
        let mut lines = out.lines();
        let window = lines.next().unwrap_or("").trim().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let latest = lines
            .filter_map(|l| l.split_once('\t'))
            .filter(|(w, _)| *w == window)
            .filter_map(|(_, at)| at.trim().parse::<f64>().ok())
            .fold(None, |m: Option<f64>, at| Some(m.map_or(at, |m| m.max(at))));
        Ok(latest.map(|at| (now - at).max(0.0)))
    }

    /// The environment new windows get (§5.6): the server's global
    /// environment, with that of `session` (if given) on top. `None` if no
    /// server runs.
    pub fn environment(&self, session: Option<&str>) -> Result<Option<BTreeMap<String, String>>> {
        let (ok, out, err) = self.try_run(&["show-environment".into(), "-g".into()])?;
        if !ok {
            if no_server(&err) {
                return Ok(None);
            }
            return Err(Error::runtime(format!(
                "tmux show-environment failed: {}",
                err.trim()
            )));
        }
        let mut env = BTreeMap::new();
        apply_environment(&mut env, &out);
        if let Some(s) = session {
            let out = self.run(&["show-environment".into(), "-t".into(), format!("={s}")])?;
            apply_environment(&mut env, &out);
        }
        Ok(Some(env))
    }

    /// Session and socket path of a pane.
    pub fn pane_session(&self, pane: &str) -> Result<(String, String)> {
        let out = self.run(&[
            "display-message".into(),
            "-p".into(),
            "-t".into(),
            pane.into(),
            "#{session_name}\t#{socket_path}".into(),
        ])?;
        let line = out.lines().next().unwrap_or("");
        let (session, socket) = line.split_once('\t').unwrap_or((line, ""));
        Ok((session.to_string(), socket.to_string()))
    }

    /// Make the pane the current one of its window and session.
    pub fn select(&self, pane: &str) -> Result<()> {
        self.run(&[
            "select-window".into(),
            "-t".into(),
            pane.into(),
            ";".into(),
            "select-pane".into(),
            "-t".into(),
            pane.into(),
        ])
        .map(|_| ())
    }

    /// Replace this process with `tmux attach-session -t =<session>`.
    pub fn exec_attach(&self, session: &str) -> Error {
        use std::os::unix::process::CommandExt;
        let e = self
            .command(&["attach-session".into(), "-t".into(), format!("={session}")])
            .exec();
        Error::runtime(format!("cannot run tmux: {e}"))
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

/// Apply the output of `show-environment` (`NAME=VALUE`, or `-NAME` for a
/// variable removed from new windows).
fn apply_environment(env: &mut BTreeMap<String, String>, out: &str) {
    for line in out.lines() {
        if let Some(name) = line.strip_prefix('-') {
            env.remove(name);
        } else if let Some((k, v)) = line.split_once('=') {
            env.insert(k.to_string(), v.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_lines() {
        let mut env = BTreeMap::new();
        apply_environment(&mut env, "HOME=/h\nA=1\nB=x=y\n");
        apply_environment(&mut env, "-A\nC=3\n");
        let got: Vec<_> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert_eq!(got, ["B=x=y", "C=3", "HOME=/h"]);
    }

    #[test]
    fn script_words() {
        assert_eq!(script_word("a b"), "'a b'");
        assert_eq!(script_word("it's"), "'it'\"'\"'s'");
        assert_eq!(
            script_line(&[
                vec!["new-session".into(), "-d".into()],
                vec!["set-environment".into(), "A".into(), "$x;".into()],
            ]),
            "'new-session' '-d' ; 'set-environment' 'A' '$x;'"
        );
    }

    #[test]
    fn escaping() {
        assert_eq!(tq("it's"), "'it'\\''s'");
    }
}
