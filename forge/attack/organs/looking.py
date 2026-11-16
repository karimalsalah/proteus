r"""Does looking slow the looked-at? Every look at a session this server did not spawn asks the daemon
for its list, and the daemon then writes every session's journal out. A flood of 10 MB is timed alone,
with a second server blocked in one wait on it (a look every 250 ms), and with a second server calling
screen as fast as it can; twice each, in turn, so that a busy machine shows as scatter and not as an effect.

    python forge\attack\organs\looking.py
"""
import sys, time, json, os, threading
from mcpc import *

h = Home("looking")
try:
    owner = Client(h); owner.handshake()
    guest = Client(h); guest.handshake()
    big = os.path.join(h.dir, "big.txt")
    with open(big, "w", newline="") as fh:
        i = 0
        while fh.tell() < 10 << 20:
            fh.write(f"{i:07} the quick brown fox jumps over the lazy dog 0123456789 abcdefghijklmnopqrstuvwxyz\r\n")
            i += 1
    rates = {"alone": [], "one wait": [], "screen in a loop": []}
    looks = []
    for rep in range(2):
        for how in rates:
            f, _ = owner.ok("spawn", {"command": "cmd.exe /d", "cwd": h.dir})
            sid = f["session"]
            owner.ok("wait", {"session": sid, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
            stop = threading.Event()
            n = [0]

            def look():
                if how == "one wait":
                    guest.call("wait", {"session": sid, "until": {"regex": "never-printed-xyz"}, "timeout_ms": 60000}, timeout=90)
                elif how == "screen in a loop":
                    while not stop.is_set():
                        guest.call("screen", {"session": sid}, timeout=60)
                        n[0] += 1

            th = threading.Thread(target=look, daemon=True)
            if how != "alone":
                th.start()
                time.sleep(0.3)
            t = time.perf_counter()
            owner.ok("send", {"session": sid, "text": "type big.txt & echo FLOOD-OVER", "enter": True})
            w, _ = owner.ok("wait", {"session": sid, "until": {"regex": "^FLOOD-OVER"}, "timeout_ms": 300000}, timeout=320)
            secs = time.perf_counter() - t
            stop.set()
            rates[how].append(round(w["cursor"] / 1e6 / secs, 2))
            if how == "screen in a loop":
                looks.append(int(n[0] / secs))
            owner.ok("kill", {"session": sid})     # ends the second server's wait too
            if how != "alone":
                th.join(90)
    print("MEASURE", json.dumps({"what": "flood of 10 MB, MB/s, by who else was looking", "alone": rates["alone"], "one wait by a second server": rates["one wait"],
                                 "screen in a loop by a second server": rates["screen in a loop"], "screens_per_s": looks}))
    owner.close(); guest.close()
finally:
    h.close()
