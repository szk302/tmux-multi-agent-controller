//! Agent records (§4.2): `agent.yml`, `state.yml`, `holds/`, `input.yml`
//! and `events.log`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};
use crate::home::Home;
use crate::project::AgentRef;
use crate::util;

/// Agent state (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Starting,
    Idle,
    Busy,
    Waiting,
    Exited,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Idle => "idle",
            State::Busy => "busy",
            State::Waiting => "waiting",
            State::Exited => "exited",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        match s {
            "starting" => Some(State::Starting),
            "idle" => Some(State::Idle),
            "busy" => Some(State::Busy),
            "waiting" => Some(State::Waiting),
            "exited" => Some(State::Exited),
            _ => None,
        }
    }
}

/// tmux location of the agent (hints; the pane tag is authoritative, §3.5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TmuxInfo {
    /// `-L` socket name used at spawn, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_name: Option<String>,
    /// Socket path of the server that runs the pane (`-S`), once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

/// `agent.yml`: facts fixed at spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentFile {
    pub project: String,
    pub name: String,
    pub instance: String,
    pub driver: String,
    pub created_at: String,
    pub spawn_timeout: f64,
    pub cwd: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub env_names: Vec<String>,
    pub tmux: TmuxInfo,
    #[serde(default)]
    pub relay: BTreeMap<String, String>,
    pub human_hold: f64,
}

/// `state.yml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub state: State,
    pub since: String,
    pub seq: u64,
    pub stop_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i64>,
}

impl StateFile {
    pub fn new(state: State) -> Self {
        StateFile {
            state,
            since: util::now_stamp(),
            seq: 0,
            stop_seq: 0,
            last_event: None,
            last_event_at: None,
            exit_status: None,
        }
    }
}

/// `holds/<key>.yml` (§9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldFile {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: String,
    /// `unconfirmed` only: hash and length of the input that was not confirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
}

/// Progress of an input (§7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Progress {
    Pending,
    Submitted,
    Confirmed,
    Rejected,
}

/// `input.yml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputFile {
    pub id: String,
    pub hash: String,
    pub length: usize,
    pub at: String,
    pub progress: Progress,
}

pub const HUMAN: &str = "human";
pub const UNCONFIRMED: &str = "unconfirmed";

/// A loaded agent record.
#[derive(Debug, Clone)]
pub struct Agent {
    pub home: Home,
    pub dir: PathBuf,
    pub info: AgentFile,
}

fn read_yaml<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match util::read_opt(path)? {
        None => Ok(None),
        Some(text) => serde_yaml_ng::from_str(&text)
            .map(Some)
            .map_err(|e| Error::runtime(format!("cannot parse {}: {e}", path.display()))),
    }
}

pub fn write_yaml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_yaml_ng::to_string(value)
        .map_err(|e| Error::runtime(format!("cannot serialize {}: {e}", path.display())))?;
    util::write_atomic(path, text.as_bytes())
}

impl Agent {
    /// Load the record of an agent; `None` if there is none.
    pub fn load(home: &Home, agent: &AgentRef) -> Result<Option<Agent>> {
        let dir = home.agent_dir(agent);
        let Some(info) = read_yaml::<AgentFile>(&dir.join("agent.yml"))? else {
            return Ok(None);
        };
        Ok(Some(Agent {
            home: home.clone(),
            dir,
            info,
        }))
    }

    /// Load and require a record (exit code 4 if missing).
    pub fn require(home: &Home, agent: &AgentRef) -> Result<Agent> {
        Agent::load(home, agent)?.ok_or_else(|| Error::no_agent(&agent.to_string()))
    }

    /// Name within the project.
    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn project(&self) -> &str {
        &self.info.project
    }

    pub fn key(&self) -> AgentRef {
        AgentRef {
            project: self.info.project.clone(),
            name: self.info.name.clone(),
        }
    }

    /// `project/name`, for messages.
    pub fn label(&self) -> String {
        self.key().to_string()
    }

    pub fn instance(&self) -> &str {
        &self.info.instance
    }

    /// Whether the record on disk still belongs to this instance.
    pub fn still_current(&self) -> Result<bool> {
        Ok(Agent::load(&self.home, &self.key())?
            .is_some_and(|a| a.info.instance == self.info.instance))
    }

    pub fn write_info(&self) -> Result<()> {
        write_yaml(&self.dir.join("agent.yml"), &self.info)
    }

    pub fn state(&self) -> Result<StateFile> {
        read_yaml(&self.dir.join("state.yml"))?
            .ok_or_else(|| Error::runtime(format!("missing state.yml for {}", self.label())))
    }

    pub fn write_state(&self, st: &StateFile) -> Result<()> {
        write_yaml(&self.dir.join("state.yml"), st)
    }

    fn holds_dir(&self) -> PathBuf {
        self.dir.join("holds")
    }

    pub fn hold(&self, key: &str) -> Result<Option<HoldFile>> {
        read_yaml(&self.holds_dir().join(format!("{key}.yml")))
    }

    pub fn holds(&self) -> Result<Vec<HoldFile>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(self.holds_dir()) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let path = entry?.path();
            let is_hold = path.extension().is_some_and(|x| x == "yml")
                && !path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'));
            if is_hold && let Some(h) = read_yaml::<HoldFile>(&path)? {
                out.push(h);
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    pub fn write_hold(&self, hold: &HoldFile) -> Result<()> {
        fs::create_dir_all(self.holds_dir())?;
        write_yaml(&self.holds_dir().join(format!("{}.yml", hold.key)), hold)
    }

    pub fn remove_hold(&self, key: &str) -> Result<bool> {
        let path = self.holds_dir().join(format!("{key}.yml"));
        let existed = path.exists();
        util::remove_opt(&path)?;
        Ok(existed)
    }

    pub fn input(&self) -> Result<Option<InputFile>> {
        read_yaml(&self.dir.join("input.yml"))
    }

    pub fn write_input(&self, input: &InputFile) -> Result<()> {
        write_yaml(&self.dir.join("input.yml"), input)
    }

    pub fn remove_input(&self) -> Result<()> {
        util::remove_opt(&self.dir.join("input.yml"))
    }

    /// Append a line to `events.log`: time, sequence, source, event, change.
    pub fn log(&self, seq: u64, source: &str, event: &str, change: &str) {
        let line = format!("{} {seq} {source} {event} {change}", util::now_stamp());
        if let Err(e) = util::append_line(&self.dir.join("events.log"), &line) {
            crate::error::warn(format!("cannot write events.log of {}: {e}", self.label()));
        }
    }

    /// Change the state (caller holds the state lock). `since` changes only
    /// when the state changes. Returns the previous state.
    pub fn transition(
        &self,
        st: &mut StateFile,
        to: State,
        source: &str,
        event: &str,
    ) -> Result<State> {
        let from = st.state;
        if from == State::Exited {
            return Ok(from);
        }
        st.seq += 1;
        if from != to {
            st.state = to;
            st.since = util::now_stamp();
        }
        st.last_event = Some(event.to_string());
        st.last_event_at = Some(util::now_stamp());
        self.write_state(st)?;
        let change = if from == to {
            from.as_str().to_string()
        } else {
            format!("{}->{}", from.as_str(), to.as_str())
        };
        self.log(st.seq, source, event, &change);
        Ok(from)
    }

    /// Record an event that does not change the state.
    pub fn touch(&self, st: &mut StateFile, source: &str, event: &str) -> Result<()> {
        st.seq += 1;
        st.last_event = Some(event.to_string());
        st.last_event_at = Some(util::now_stamp());
        self.write_state(st)?;
        self.log(st.seq, source, event, st.state.as_str());
        Ok(())
    }

    /// Remove a `human` hold that has run out, so that it does not come back
    /// the next time the agent becomes idle (caller holds the state lock).
    pub fn expire_holds(&self, st: &StateFile) -> Result<bool> {
        match self.hold(HUMAN)? {
            Some(h) if human_expired(&h, st, self.info.human_hold) => self.remove_hold(HUMAN),
            _ => Ok(false),
        }
    }

    /// Keys of holds in effect now (§9.2, §9.3).
    pub fn active_holds(&self, st: &StateFile) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for h in self.holds()? {
            if h.key == HUMAN && human_expired(&h, st, self.info.human_hold) {
                continue;
            }
            out.push(h.key);
        }
        Ok(out)
    }
}

/// A `human` hold ends `human_hold` seconds after the agent became idle
/// following the hold (§9.3).
fn human_expired(h: &HoldFile, st: &StateFile, human_hold: f64) -> bool {
    if st.state != State::Idle {
        return false;
    }
    let (Some(idle_since), Some(set_at)) = (
        util::parse_stamp(&st.since),
        util::parse_stamp(&h.created_at),
    ) else {
        return false;
    };
    idle_since >= set_at && util::elapsed_secs(&st.since) >= human_hold
}

/// Create a record in `.creating/` and move it into place (§4.3, §5.3).
pub fn create(home: &Home, info: &AgentFile, driver_files: &[(String, Vec<u8>)]) -> Result<Agent> {
    let staging = home
        .creating()
        .join(format!("{}.{}.{}", info.project, info.name, info.instance));
    fs::create_dir_all(staging.join("holds"))?;
    fs::create_dir_all(staging.join("driver"))?;
    let r = (|| -> Result<()> {
        write_yaml(&staging.join("agent.yml"), info)?;
        write_yaml(&staging.join("state.yml"), &StateFile::new(State::Starting))?;
        for (name, data) in driver_files {
            util::write_atomic(&staging.join("driver").join(name), data)?;
        }
        Ok(())
    })();
    if let Err(e) = r {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    let key = AgentRef {
        project: info.project.clone(),
        name: info.name.clone(),
    };
    let dest = home.agent_dir(&key);
    // Project directories are never removed, so a concurrent spawn in the
    // same project cannot lose its parent directory.
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Err(e) = util::rename_noreplace(&staging, &dest) {
        let _ = fs::remove_dir_all(&staging);
        if util::is_exists(&e) {
            return Err(Error::runtime(format!("agent already exists: {key}")));
        }
        return Err(Error::runtime(format!(
            "cannot create {}: {e}",
            dest.display()
        )));
    }
    Ok(Agent {
        home: home.clone(),
        dir: dest,
        info: info.clone(),
    })
}

/// Remove a record by moving it to `.removing/` first (§4.3).
pub fn remove(agent: &Agent) -> Result<()> {
    fs::create_dir_all(agent.home.removing())?;
    let staging = agent.home.removing().join(format!(
        "{}.{}.{}",
        agent.project(),
        agent.name(),
        agent.instance()
    ));
    match fs::rename(&agent.dir, &staging) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(Error::runtime(format!(
                "cannot remove {}: {e}",
                agent.dir.display()
            )));
        }
    }
    fs::remove_dir_all(&staging)
        .map_err(|e| Error::runtime(format!("cannot remove {}: {e}", staging.display())))
}

fn valid_entries(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if util::valid_name(&name) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Agents of one project, or of all projects, sorted.
pub fn list(home: &Home, project: Option<&str>) -> Result<Vec<AgentRef>> {
    let projects = match project {
        Some(p) => vec![p.to_string()],
        None => valid_entries(&home.agents())?,
    };
    let mut out = Vec::new();
    for p in projects {
        for name in valid_entries(&home.agents().join(&p))? {
            out.push(AgentRef {
                project: p.clone(),
                name,
            });
        }
    }
    Ok(out)
}
