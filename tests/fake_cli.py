#!/usr/bin/env -S python3 -I
"""A fake AI CLI for tests. It reads the hooks tmac passes (claude:
--settings FILE, codex: -c hooks.EVENT=...), reads keys from its terminal
like a TUI with bracketed paste, and fires hooks the way the real CLIs do.

Prompts it understands:
  busy N    work for N seconds (PreToolUse ... PostToolUse)
  ask       wait for approval: PermissionRequest, then the key "y"
  crash     exit with status 3 without SessionEnd
  /exit /quit   SessionEnd, then exit 0
Anything else is answered at once. ESC (alone) interrupts (codex: Interrupt).

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

LOG = os.environ.get("FAKE_LOG")


def log(**kw):
    if LOG:
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps(kw, ensure_ascii=False) + "\n")


def parse_hooks(argv):
    """Return (mode, {event: [command, ...]}, positional args)."""
    hooks, rest, mode = {}, [], None
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--settings":
            mode = "claude"
            with open(argv[i + 1], encoding="utf-8") as f:
                settings = json.load(f)
            for ev, groups in settings.get("hooks", {}).items():
                for g in groups:
                    for h in g.get("hooks", []):
                        hooks.setdefault(ev, []).append(h["command"])
            i += 2
            continue
        if a == "-c":
            key, _, value = argv[i + 1].partition("=")
            if key.startswith("hooks."):
                mode = "codex"
                ev = key[len("hooks."):]
                for g in tomllib.loads("v = " + value)["v"]:
                    for h in g.get("hooks", []):
                        hooks.setdefault(ev, []).append(h["command"])
            i += 2
            continue
        rest.append(a)
        i += 1
    return mode, hooks, rest


MODE, HOOKS, POSITIONAL = parse_hooks(sys.argv[1:])


def fire(event, **payload):
    """Run the hooks of an event. Returns True if one blocked."""
    payload = {"hook_event_name": event, "session_id": "fake", **payload}
    blocked = False
    for cmd in HOOKS.get(event, []):
        p = subprocess.run(cmd, shell=True, input=json.dumps(payload).encode(),
                           stdout=subprocess.PIPE)
        out = p.stdout.decode(errors="replace").strip()
        decision = None
        try:
            decision = json.loads(out).get("decision") if out else None
        except (ValueError, AttributeError):
            pass
        b = p.returncode == 2 or (p.returncode == 0 and decision == "block")
        log(event=event, code=p.returncode, out=out, blocked=b)
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
    log(event="start", mode=MODE, args=sys.argv[1:])

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

    def run_turn(prompt):
        if os.environ.get("FAKE_NO_PROMPT_HOOK") != "1":
            if fire("UserPromptSubmit", prompt=prompt):
                log(event="rejected", prompt=prompt)
                return
        log(event="prompt", prompt=prompt)
        say(f"\r\n> {prompt}\r\n")
        words = prompt.split()
        if prompt in ("/exit", "/quit"):
            fire("SessionEnd", reason="prompt_input_exit")
            log(event="exit")
            term.restore()
            sys.exit(0)
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
