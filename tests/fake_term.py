#!/usr/bin/env -S python3 -I
"""A fake terminal for tests: run a command (`tmux attach`, `tmac attach`)
in a pseudo terminal, like a person's terminal. Each line on standard
input is typed into it without the line break (a person pressing keys);
the end of standard input closes the terminal."""

import os
import pty
import select
import signal
import struct
import sys
import termios
import fcntl

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp(sys.argv[1], sys.argv[1:])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
stdin = sys.stdin.fileno()
while True:
    ready, _, _ = select.select([fd, stdin], [], [], 0.2)
    if fd in ready:
        try:
            if not os.read(fd, 65536):
                break
        except OSError:
            break
    if stdin in ready:
        line = os.read(stdin, 4096)
        if not line:
            break
        os.write(fd, line.rstrip(b"\n"))
    if os.waitpid(pid, os.WNOHANG)[0]:
        break
try:
    os.kill(pid, signal.SIGHUP)
except ProcessLookupError:
    pass
