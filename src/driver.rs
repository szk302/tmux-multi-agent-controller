//! Drivers (§3.2): what differs between AI CLIs.

use std::path::Path;

use serde_json::Value;

use crate::record::State;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Claude,
    Codex,
}

/// State change before the relay runs (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Before {
    None,
    /// Move to `busy` (or `waiting`).
    To(State),
    /// `UserPromptSubmit`: match the input, detect human input, move to `busy`.
    Prompt,
    /// `Stop`: count the stop (`stop_seq`).
    Stop,
}

/// State change after the relay ran (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    None,
    To(State),
    /// `UserPromptSubmit`: confirm or reject the input.
    Prompt,
    /// `Stop`: `idle`, or `busy` if the relay kept the agent going.
    Stop,
}

const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "PermissionRequest",
    "Stop",
    "SessionEnd",
];

const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "Interrupt",
    "SessionEnd",
];

/// Events whose hooks can stop the agent from stopping or reject input,
/// which the state tracking assumes only tmac handles (§5.6).
pub const BLOCKING_EVENTS: &[&str] = &["Stop", "UserPromptSubmit"];

/// Every built-in driver.
pub const ALL: [Driver; 2] = [Driver::Claude, Driver::Codex];

fn str_field<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload.get(key).and_then(Value::as_str).unwrap_or("")
}

impl Driver {
    pub fn from_name(s: &str) -> Option<Driver> {
        match s {
            "claude" => Some(Driver::Claude),
            "codex" => Some(Driver::Codex),
            _ => None,
        }
    }

    /// Pick the driver from the file name of the command.
    pub fn from_command(program: &str) -> Option<Driver> {
        let base = Path::new(program)
            .file_name()?
            .to_string_lossy()
            .into_owned();
        Driver::from_name(&base)
    }

    pub fn name(self) -> &'static str {
        match self {
            Driver::Claude => "claude",
            Driver::Codex => "codex",
        }
    }

    /// Events tmac registers.
    pub fn events(self) -> &'static [&'static str] {
        match self {
            Driver::Claude => CLAUDE_EVENTS,
            Driver::Codex => CODEX_EVENTS,
        }
    }

    /// Default delay between paste and Enter.
    pub fn submit_delay_default_ms(self) -> u64 {
        match self {
            Driver::Claude => 200,
            Driver::Codex => 500,
        }
    }

    /// Text typed to end the CLI normally (§11).
    pub fn exit_text(self) -> &'static str {
        match self {
            Driver::Claude => "/exit",
            Driver::Codex => "/quit",
        }
    }

    /// Name of the CLI in rulesync (`tmac hooks rulesync`, §5.5).
    pub fn rulesync_target(self) -> &'static str {
        match self {
            Driver::Claude => "claudecode",
            Driver::Codex => "codexcli",
        }
    }

    /// Time limit written for tmac's hook of an event, in seconds (§5.5).
    /// 30 is the most Claude Code allows for UserPromptSubmit.
    pub fn hook_timeout(self, event: &str) -> u64 {
        if self == Driver::Codex && matches!(event, "Interrupt" | "SessionEnd") {
            // Codex clamps these to 3 seconds.
            return 3;
        }
        30
    }

    /// Time limit the CLI uses for a hook without one, in seconds; `None`
    /// if unknown.
    pub fn default_hook_timeout(self) -> Option<f64> {
        match self {
            Driver::Claude => Some(60.0),
            Driver::Codex => None,
        }
    }

    /// Session ID of the CLI in a hook input (§6.2).
    pub fn session_id(self, payload: &Value) -> Option<String> {
        payload
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// Whether the event starts another session of the same CLI (`/clear`,
    /// resuming another session), so that the agent's session ID may change
    /// (§6.2).
    pub fn starts_new_session(self, event: &str, payload: &Value) -> bool {
        event == "SessionStart" && matches!(str_field(payload, "source"), "clear" | "resume")
    }

    pub fn before(self, event: &str, payload: &Value) -> Before {
        match event {
            "UserPromptSubmit" => Before::Prompt,
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Before::To(State::Busy),
            "PermissionRequest" => Before::To(State::Waiting),
            "Notification" if self == Driver::Claude => {
                match str_field(payload, "notification_type") {
                    "permission_prompt" | "elicitation_dialog" => Before::To(State::Waiting),
                    _ => Before::None,
                }
            }
            "Stop" => Before::Stop,
            _ => Before::None,
        }
    }

    pub fn after(self, event: &str, payload: &Value) -> After {
        match event {
            "UserPromptSubmit" => After::Prompt,
            "Stop" => After::Stop,
            "SessionStart" => {
                let source = str_field(payload, "source");
                let idle = match self {
                    // Claude fires SessionStart when it is ready for input.
                    Driver::Claude => matches!(source, "startup" | "resume" | "clear"),
                    // Codex fires it with the first prompt of a session; the
                    // prompt itself follows.
                    Driver::Codex => matches!(source, "resume" | "clear"),
                };
                if idle {
                    After::To(State::Idle)
                } else {
                    After::None
                }
            }
            "Notification" if self == Driver::Claude => {
                if str_field(payload, "notification_type") == "idle_prompt" {
                    After::To(State::Idle)
                } else {
                    After::None
                }
            }
            // Esc in Codex fires Interrupt instead of Stop.
            "Interrupt" if self == Driver::Codex => After::To(State::Idle),
            "SessionEnd" => match str_field(payload, "reason") {
                "clear" | "resume" => After::None,
                _ => After::To(State::Exited),
            },
            _ => After::None,
        }
    }

    /// Whether a submitted prompt was made by the CLI itself rather than
    /// typed: Claude Code hands notices of finished background tasks to the
    /// model as prompts (`<task-notification>`) and fires UserPromptSubmit
    /// for them (§9.3).
    pub fn generated_prompt(self, prompt: &str) -> bool {
        match self {
            Driver::Claude => prompt.trim_start().starts_with("<task-notification>"),
            Driver::Codex => false,
        }
    }

    /// Whether the relay stopped the stop or rejected the input (§6.3).
    pub fn relay_blocked(self, code: i32, stdout: &[u8]) -> bool {
        if code == 2 {
            return true;
        }
        code == 0
            && serde_json::from_slice::<Value>(stdout)
                .ok()
                .and_then(|v| {
                    v.get("decision")
                        .and_then(Value::as_str)
                        .map(|d| d == "block")
                })
                .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn driver_from_command() {
        assert_eq!(
            Driver::from_command("/usr/bin/claude"),
            Some(Driver::Claude)
        );
        assert_eq!(Driver::from_command("codex"), Some(Driver::Codex));
        assert_eq!(Driver::from_command("bash"), None);
    }

    #[test]
    fn hook_values() {
        assert_eq!(Driver::Claude.hook_timeout("Stop"), 30);
        assert_eq!(Driver::Codex.hook_timeout("Interrupt"), 3);
        assert_eq!(Driver::Codex.rulesync_target(), "codexcli");
        let p = json!({"session_id": "s1", "source": "clear"});
        assert_eq!(Driver::Codex.session_id(&p).as_deref(), Some("s1"));
        assert_eq!(Driver::Claude.session_id(&json!({"session_id": ""})), None);
        assert!(Driver::Claude.starts_new_session("SessionStart", &p));
        assert!(!Driver::Claude.starts_new_session("SessionStart", &json!({"source": "startup"})));
        assert!(!Driver::Claude.starts_new_session("Stop", &p));
    }

    #[test]
    fn generated_prompts() {
        assert!(Driver::Claude.generated_prompt("<task-notification>\n<task-id>x</task-id>"));
        assert!(!Driver::Claude.generated_prompt("please look at <task-notification>"));
        assert!(!Driver::Codex.generated_prompt("<task-notification>"));
    }

    #[test]
    fn relay_block_detection() {
        let d = Driver::Claude;
        assert!(d.relay_blocked(2, b""));
        assert!(d.relay_blocked(0, br#"{"decision":"block","reason":"x"}"#));
        assert!(!d.relay_blocked(0, b"plain"));
        assert!(!d.relay_blocked(1, br#"{"decision":"block"}"#));
    }

    #[test]
    fn event_mapping() {
        let p = json!({"source": "startup"});
        assert_eq!(
            Driver::Claude.after("SessionStart", &p),
            After::To(State::Idle)
        );
        assert_eq!(Driver::Codex.after("SessionStart", &p), After::None);
        assert_eq!(
            Driver::Codex.after("Interrupt", &json!({})),
            After::To(State::Idle)
        );
        assert_eq!(
            Driver::Claude.before(
                "Notification",
                &json!({"notification_type": "permission_prompt"})
            ),
            Before::To(State::Waiting)
        );
        assert_eq!(
            Driver::Claude.after("SessionEnd", &json!({"reason": "clear"})),
            After::None
        );
        assert_eq!(
            Driver::Claude.after("SessionEnd", &json!({"reason": "logout"})),
            After::To(State::Exited)
        );
    }
}
