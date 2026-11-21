#!/usr/bin/env -S python3 -I
"""A fake AI CLI for tests. Run it as `claude` or `codex` (a link with that
name). Like the real CLIs, it reads hooks from the user's settings (claude:
$CLAUDE_CONFIG_DIR/settings.json, codex: $CODEX_HOME/hooks.json), the
project's settings and the caller's arguments (claude: --settings, codex:
-c hooks.EVENT=...), runs them with the shell, reads keys from its terminal
like a TUI with bracketed paste, and fires hooks the way the real CLIs do.

Prompts it understands:
  busy N    work for N seconds (PreToolUse ... PostToolUse)
  ask       wait for approval: PermissionRequest, then the key "y"
  nested    like a CLI started by the agent's tool: hooks of another session
            and of the other driver fire while the agent works
  crash     exit with status 3 without SessionEnd
  /clear    start a new session (claude: SessionEnd and SessionStart now;
            codex: SessionStart before the next prompt, no SessionEnd)
  /exit /quit   SessionEnd, then exit 0 (codex: also for earlier sessions)
Anything else is answered at once. ESC (alone) interrupts (codex: Interrupt).
Slash commands do not fire UserPromptSubmit.

Environment:
  FAKE_LOG               file to append events to (one JSON object per line)
  FAKE_NO_PROMPT_HOOK=1  do not fire UserPromptSubmit
"""

import json
import os
import select
import subprocess
import sys
import termios
import time
import tomllib
import tty
import uuid

LOG = os.environ.get("FAKE_LOG")


def log(**kw):
    if LOG:
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps(kw, ensure_ascii=False) + "\n")


def add_hooks(hooks, obj):
    for ev, groups in (obj or {}).items():
        for g in groups:
            for h in g.get("hooks", [g]):
                if "command" in h:
                    hooks.setdefault(ev, []).append(h["command"])


def read_json(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def parse_hooks(mode, argv):
    """Return ({event: [command, ...]}, positional args)."""
    hooks, rest = {}, []
    if mode == "claude":
        add_hooks(hooks, read_json(os.path.join(os.environ.get(
            "CLAUDE_CONFIG_DIR", os.path.expanduser("~/.claude")), "settings.json")).get("hooks"))
        for name in ("settings.json", "settings.local.json"):
            add_hooks(hooks, read_json(os.path.join(".claude", name)).get("hooks"))
    else:
        add_hooks(hooks, read_json(os.path.join(os.environ.get(
            "CODEX_HOME", os.path.expanduser("~/.codex")), "hooks.json")).get("hooks"))
        add_hooks(hooks, read_json(os.path.join(".codex", "hooks.json")).get("hooks"))
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--settings" and mode == "claude":
            v = argv[i + 1]
            add_hooks(hooks, (json.loads(v) if v.lstrip().startswith("{")
                              else read_json(v)).get("hooks"))
            i += 2
            continue
        if a == "-c" and mode == "codex":
            key, _, value = argv[i + 1].partition("=")
            if key.startswith("hooks."):
                add_hooks(hooks, {key[len("hooks."):]: tomllib.loads("v = " + value)["v"]})
            i += 2
            continue
        rest.append(a)
        i += 1
    return hooks, rest


MODE = os.path.basename(sys.argv[0])
HOOKS, POSITIONAL = parse_hooks(MODE, sys.argv[1:])
SESSION = str(uuid.uuid4())
OLD_SESSIONS = []


def run_hook(cmd, payload):
    p = subprocess.run(cmd, shell=True, input=json.dumps(payload).encode(),
                       stdout=subprocess.PIPE)
    return p.returncode, p.stdout.decode(errors="replace").strip()


def fire(event, session=None, **payload):
    """Run the hooks of an event. Returns True if one blocked."""
    payload = {"hook_event_name": event, "session_id": session or SESSION, **payload}
    blocked = False
    for cmd in HOOKS.get(event, []):
        code, out = run_hook(cmd, payload)
        decision = None
        try:
            decision = json.loads(out).get("decision") if out else None
        except (ValueError, AttributeError):
            pass
        b = code == 2 or (code == 0 and decision == "block")
        log(event=event, code=code, out=out, blocked=b, session=payload["session_id"])
        blocked |= b
    return blocked


def say(text):
    sys.stdout.write(text.replace("\n", "\r\n"))
    sys.stdout.flush()


class Term:
    def __init__(self):
        self.fd = sys.stdin.fileno()
        self.old = termios.tcgetattr(self.fd)
        tty.setraw(self.fd)
        say("\x1b[?2004h")  # ask the terminal for bracketed paste

    def restore(self):
        termios.tcsetattr(self.fd, termios.TCSADRAIN, self.old)

    def read(self, timeout=None):
        r, _, _ = select.select([self.fd], [], [], timeout)
        return os.read(self.fd, 4096) if r else b""


class Input:
    """Turns raw bytes into ('prompt', text) and ('esc', None) events."""

    def __init__(self):
        self.buf = b""
        self.line = b""
        self.paste = False

    def feed(self, data):
        self.buf += data
        events = []
        while self.buf:
            if self.paste:
                end = self.buf.find(b"\x1b[201~")
                if end < 0:
                    self.line += self.buf.replace(b"\r", b"\n")
                    self.buf = b""
                    break
                self.line += self.buf[:end].replace(b"\r", b"\n")
                self.buf = self.buf[end + 6:]
                self.paste = False
                continue
            if self.buf.startswith(b"\x1b[200~"):
                self.paste = True
                self.buf = self.buf[6:]
                continue
            if self.buf.startswith(b"\x1b"):
                if len(self.buf) == 1:
                    events.append(("esc", None))
                    self.buf = b""
                    continue
                self.buf = self.buf[1:]
                continue
            c, self.buf = self.buf[:1], self.buf[1:]
            if c in (b"\r", b"\n"):
                events.append(("prompt", self.line.decode(errors="replace")))
                self.line = b""
            elif c == b"\x7f":
                self.line = self.line[:-1]
            else:
                self.line += c
        return events


def main():
    term = Term()
    inp = Input()
    log(event="start", mode=MODE, args=sys.argv[1:],
        env={k: v for k, v in os.environ.items()
             if k.startswith(("CLAUDE", "CODEX", "TMAC_"))})

    def wait_for_key(key, seconds):
        """Wait for a key, or ESC (returns 'esc')."""
        end = time.time() + seconds
        while time.time() < end:
            data = term.read(max(0.0, end - time.time()))
            for kind, _ in inp.feed(data):
                if kind == "esc":
                    return "esc"
            if key and key.encode() in data:
                return "key"
        return "timeout"

    pending_start = [None]

    def new_session():
        global SESSION
        OLD_SESSIONS.append(SESSION)
        SESSION = str(uuid.uuid4())

    def run_turn(prompt):
        if pending_start[0]:
            fire("SessionStart", source=pending_start[0])
            pending_start[0] = None
        slash = prompt.startswith("/")
        if os.environ.get("FAKE_NO_PROMPT_HOOK") != "1" and not slash:
            if fire("UserPromptSubmit", prompt=prompt):
                log(event="rejected", prompt=prompt)
                return
        log(event="prompt", prompt=prompt)
        say(f"\r\n> {prompt}\r\n")
        words = prompt.split()
        if prompt in ("/exit", "/quit"):
            if MODE == "codex":
                for s in OLD_SESSIONS:
                    fire("SessionEnd", session=s, reason="other")
            fire("SessionEnd", reason="prompt_input_exit")
            log(event="exit")
            term.restore()
            sys.exit(0)
        if prompt == "/clear":
            if MODE == "codex":
                new_session()
                pending_start[0] = "clear"
            else:
                fire("SessionEnd", reason="clear")
                new_session()
                fire("SessionStart", source="clear")
            log(event="cleared", session=SESSION)
            return
        if words[:1] == ["crash"]:
            log(event="crash")
            term.restore()
            sys.exit(3)
        if words[:1] == ["busy"]:
            fire("PreToolUse", tool_name="Bash")
            if wait_for_key(None, float(words[1])) == "esc":
                interrupted()
                return
            fire("PostToolUse", tool_name="Bash")
        if words[:1] == ["nested"]:
            fire("PreToolUse", tool_name="Bash")
            # A CLI the tool started: same driver, another session ...
            fire("SessionStart", session="nested", source="startup")
            fire("Stop", session="nested", stop_hook_active=False)
            # ... and the other driver, with the agent's environment.
            other = "codex" if MODE == "claude" else "claude"
            code, _ = run_hook(f"command -v tmac >/dev/null 2>&1 || exit 0; "
                               f"exec tmac hook {other} Stop",
                               {"hook_event_name": "Stop", "session_id": "other"})
            log(event="nested", code=code)
            fire("PostToolUse", tool_name="Bash")
        if words[:1] == ["selfnote"]:
            # Like Claude Code delivering a background task notice mid-turn.
            fire("UserPromptSubmit", prompt="<task-notification>\n<task-id>t1</task-id>\n"
                 "<status>completed</status>\n</task-notification>")
        if words[:1] == ["ask"]:
            fire("PermissionRequest", tool_name="Bash")
            if wait_for_key("y", 60) == "esc":
                interrupted()
                return
            fire("PostToolUse", tool_name="Bash")
        active = False
        for _ in range(3):
            if not fire("Stop", stop_hook_active=active):
                break
            log(event="continued")
            active = True
        say("done\r\n")

    def interrupted():
        log(event="interrupted")
        if MODE == "codex":
            fire("Interrupt")

    if MODE == "codex":
        if POSITIONAL:
            fire("SessionStart", source="startup")
            run_turn(POSITIONAL[-1])
    else:
        fire("SessionStart", source="startup")
    while True:
        data = term.read(None)
        for kind, text in inp.feed(data):
            if kind == "prompt" and text.strip():
                run_turn(text)
            elif kind == "esc":
                interrupted()


if __name__ == "__main__":
    main()
