"""Are late wait timeouts the server's doing or the machine's? A thread in this client sleeps 10 ms over and over
and records its own overshoot at the same time."""
import sys, time, json, threading
from mcpc import *

h = Home("lateness")
stalls = []
stop = False


def metronome():
    while not stop:
        t = time.perf_counter()
        time.sleep(0.01)
        over = (time.perf_counter() - t - 0.01) * 1000
        if over > 20:
            stalls.append((round(t - T0, 2), round(over, 1)))


try:
    T0 = time.perf_counter()
    threading.Thread(target=metronome, daemon=True).start()
    owner = Client(h); owner.handshake()
    f, _ = owner.ok("spawn", {"command": "cmd.exe /d"})
    sid = f["session"]
    owner.ok("wait", {"session": sid, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
    guest = Client(h); guest.handshake(); guest.ok("screen", {"session": sid})
    for rnd in range(2):
        for who, c in (("spawned here", owner), ("another server's", guest)):
            late = []; inner = []; outliers = []
            for _ in range(150):
                t = time.perf_counter()
                f, _ = c.ok("wait", {"session": sid, "until": {"regex": "never-printed-xyz"}, "timeout_ms": 100})
                l = (time.perf_counter() - t) * 1000 - 100
                late.append(l); inner.append(f["elapsed_ms"] - 100)
                if l > 30:
                    outliers.append((round(t - T0, 2), round(l, 1), f["elapsed_ms"] - 100))
            print("MEASURE", json.dumps({"what": "wait timeout 100 ms: ms past it", "session": who, "round": rnd, "client": summary(late), "server": summary(inner), "outliers(at s, client late, server late)": outliers}))
    stop = True
    print("MEASURE", json.dumps({"what": "this client's own 10 ms sleeps that overshot by more than 20 ms (at s, ms over)", "stalls": stalls}))
    owner.close(); guest.close()
finally:
    h.close()
