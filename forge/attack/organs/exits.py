r"""How long does the server take to leave once its input is closed, and does it matter what it did before?
Each round starts a server, lets it do one thing, closes its input and times its exit. A server that has
drawn a picture holds a GPU device when it leaves; one that has not, does not.

    python forge\attack\organs\exits.py [rounds]
"""
import sys, time, json, os, subprocess
from mcpc import *

rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 15
busy = int(sys.argv[2]) if len(sys.argv) > 2 else 0      # other servers drawing pictures all the while
h = Home("exits")
try:
    keeper = Client(h); keeper.handshake()
    f, _ = keeper.ok("spawn", {"command": "cmd.exe /d /c echo something to look at"})
    sid = f["session"]
    keeper.ok("wait", {"session": sid, "until": {"exit": True}, "timeout_ms": 30000})
    out = {}
    import threading
    stop = threading.Event()
    drawn = [0]

    def draw():
        c = Client(h); c.handshake()
        while not stop.is_set():
            c.call("shot", {"session": sid, "exact": True, "scale": 2}, timeout=120)
            drawn[0] += 1
        c.close()

    others = [threading.Thread(target=draw, daemon=True) for _ in range(busy)]
    for t in others:
        t.start()
    if busy:
        time.sleep(2)
    for what in ("nothing", "screen", "shot", "shot, then 2 s"):
        times, stuck = [], 0
        for i in range(rounds):
            c = Client(h); c.handshake()
            if what == "screen":
                c.ok("screen", {"session": sid})
            elif what.startswith("shot"):
                res, facts, _ = c.call("shot", {"session": sid})
                assert res and res.get("isError") is False, facts
                if what != "shot":
                    time.sleep(2)
            t = time.perf_counter()
            c.p.stdin.close()
            try:
                c.p.wait(30)
                times.append((time.perf_counter() - t) * 1000)
            except subprocess.TimeoutExpired:
                stuck += 1
                c.p.kill()      # our own child, by handle
        out[what] = {"ms": summary(times) if times else None, "still_there_after_30_s": stuck}
        print("MEASURE", json.dumps({"what": "server exit after its input closed", "it had done": what, "other servers drawing meanwhile": busy, **out[what]}))
    stop.set()
    for t in others:
        t.join(60)
    if busy:
        print("pictures drawn by the others meanwhile:", drawn[0])
    keeper.close()
finally:
    h.close()
