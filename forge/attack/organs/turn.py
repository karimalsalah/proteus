r"""A pattern that is quick on one kind of text and slow on another: does a wait still end at its timeout
when the output turns from the one to the other? A megabyte the pattern rejects at a glance, then 300 KB it
cannot. (A search paced by how fast the text before was searched gives the slow text a piece sized for the
quick text. Measured at a6b76dd: a 300 ms timeout answered after 51,573 ms.)

    python forge\attack\organs\turn.py [quick_kb slow_kb timeout_ms]
"""
import sys, time, json, os
from mcpc import *

quick_kb, slow_kb, T = (int(x) for x in (sys.argv[1:4] + ["1024", "300", "300"][len(sys.argv) - 1:]))
SLOW = r"(?:(?:(?:a{1,30}){1,30}){1,30})Q"
h = Home("turn")
try:
    c = Client(h); c.handshake()
    path = os.path.join(h.dir, "turn.txt")
    with open(path, "w", newline="") as fh:
        while fh.tell() < quick_kb << 10:
            fh.write("x" * 90 + " \r\n")       # no letter the pattern could start on: rejected at once
        while fh.tell() < (quick_kb + slow_kb) << 10:
            fh.write("a" * 90 + " \r\n")       # every position is a start the pattern must follow
    f, _ = c.ok("spawn", {"command": "cmd.exe /d /c type turn.txt", "cwd": h.dir})
    sid = f["session"]
    w, _ = c.ok("wait", {"session": sid, "until": {"exit": True}, "timeout_ms": 120000}, timeout=130)
    end = w["cursor"]
    for label, cursor in (("quick text, then slow text", 0), ("slow text only", None)):
        if cursor is None:
            # where the a-lines start in the output: find it with an ordinary pattern
            m, _ = c.ok("wait", {"session": sid, "until": {"regex": "^a{90} $"}, "timeout_ms": 20000, "cursor": 0})
            cursor = m["cursor"]
        t = time.perf_counter()
        res, facts, _ = c.call("wait", {"session": sid, "until": {"regex": SLOW}, "timeout_ms": T, "cursor": cursor}, timeout=900)
        wall = (time.perf_counter() - t) * 1000
        facts = facts if isinstance(facts, dict) else {"reason": str(facts)[:80]}
        print("MEASURE", json.dumps({"what": "slow pattern, wait with timeout_ms %d" % T, "text": label, "output_bytes": end, "from": cursor,
                                     "answered_after_ms": round(wall), "late_ms": round(wall - T), "reason": facts.get("reason"),
                                     "searched_to": facts.get("cursor"), "pattern_reach": facts.get("pattern_reach")}))
        print(("PASS " if res is not None and wall < T + 1000 else "FAIL ") + f"{label}: timeout honoured within 1 s")
    c.close()
finally:
    h.close()
