# Drives a real terminal session: Ctrl-Z, jobs, bg, kill %1, Ctrl-C, arrays.
#   cargo build --release && python3 tests/interactive.py
import os, pty, time, select, sys
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "dumb"
    os.execv(os.path.abspath("target/release/whiplash"), ["whiplash", "--norc"])
out = b""
def read(t=0.6):
    global out
    end = time.time() + t
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: out += os.read(fd, 4096)
            except OSError: return
def send(s, t=0.6):
    os.write(fd, s.encode()); read(t)
read(1)
send("sleep 30\r", 0.8)
send("\x1a", 0.8)          # Ctrl-Z
send("jobs\r")
send("bg\r")
send("jobs\r")
send("kill %1\r")
send("\r", 0.5)
send("sleep 0.3 | cat\r", 1)
send("echo status=$?\r")
send("arr=(a b c); echo ${#arr[@]} ${arr[1]}\r")
send("sleep 30\r", 0.8)
send("\x03", 0.8)          # Ctrl-C
send("echo after-ctrl-c $?\r")
send("exit\r", 0.5)
import re
text = re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", out).decode(errors="replace")
print("\n".join(l.rstrip() for l in text.replace("\r","").split("\n") if l.strip()))
