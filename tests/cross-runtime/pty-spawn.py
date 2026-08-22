#!/usr/bin/env python3
# Run a command inside a pseudo-terminal and relay its output — a port of
# Node's tools/pseudo-tty.py, which is how Node's own runner executes
# test/pseudo-tty/. Same termios tweaks (no ONLCR, no ECHOCTL), same control-
# character filter on the way out, stdin relayed into the pty (the runner feeds
# a test's `.in` file through it). Unlike Node's helper this one exits with the
# child's status, because the harness reads exit codes.
import errno
import os
import pty
import select
import signal
import sys
import termios

STDIN, STDOUT = 0, 1


def pipe(sfd, dfd):
    try:
        data = os.read(sfd, 256)
    except OSError as e:
        if e.errno != errno.EIO:
            raise
        return True
    if not data:
        return True
    if dfd == STDOUT:
        data = bytes(filter(lambda c: c >= 0x20 or c in b"\t\n\r\f", data))
    while data:
        try:
            n = os.write(dfd, data)
        except OSError as e:
            if e.errno != errno.EIO:
                raise
            return True
        data = data[n:]
    return False


def main(argv):
    signal.signal(signal.SIGCHLD, lambda *_: None)
    parent_fd, child_fd = pty.openpty()
    mode = termios.tcgetattr(child_fd)
    mode[1] &= ~termios.ONLCR
    mode[3] &= ~termios.ECHOCTL
    termios.tcsetattr(child_fd, termios.TCSANOW, mode)
    pid = os.fork()
    if pid == 0:
        os.setsid()
        os.close(parent_fd)
        fd = os.open(os.ttyname(child_fd), os.O_RDWR)
        os.dup2(fd, child_fd)
        os.close(fd)
        for target in (0, 1, 2):
            os.dup2(child_fd, target)
        if child_fd > 2:
            os.close(child_fd)
        os.execvpe(argv[0], argv, os.environ)
    os.close(child_fd)
    fds = [STDIN, parent_fd]
    while fds:
        try:
            rfds, _, _ = select.select(fds, [], [])
        except InterruptedError:
            if os.waitpid(pid, os.WNOHANG)[0] == pid:
                break
            continue
        if STDIN in rfds and pipe(STDIN, parent_fd):
            fds.remove(STDIN)
        if parent_fd in rfds and pipe(parent_fd, STDOUT):
            break
    _, status = os.waitpid(pid, 0)
    return os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    code = main(sys.argv[1:])
    sys.exit(code if code >= 0 else 128 - code)
