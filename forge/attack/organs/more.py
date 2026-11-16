r"""Second pass of the review (2026-11-16): what the first pass did not try.

  1. An error in the middle of one very long line: is the word that makes it an error still in the lean text?
  2. A message on standard input that never ends its line: how much memory does the server take?
  3. A program that repeats a character with ESC[<n>b until more rows have scrolled past than the lean
     reader's emulator keeps: is an error line printed before the flood still reported?
  4. screen, read and wait from a second server, all the time, while 20 MB floods through: errors, cursor order.

    python forge\attack\organs\more.py
"""
import sys, time, json, os, threading
from mcpc import *

h = Home("more")
findings = []
PY = sys.executable


def note(tag, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + tag + (": " + str(detail)[:600] if detail != "" else ""))
    if not ok:
        findings.append((tag, str(detail)[:900]))


try:
    c = Client(h); c.handshake()

    # ---- 1. an error in the middle of a long line ---------------------------------
    long = os.path.join(h.dir, "long.txt")
    with open(long, "w", newline="") as fh:
        for i in range(70):
            fh.write(f"step {'abcdefg'[i % 7] * (1 + i % 5)} ok\r\n")
        fh.write('{"ts":"2026-11-16T05:00:00Z","pad":"' + "y" * 2000 + '","msg":"error: disk full on /dev/sda1","trail":"' + "z" * 3000 + '"}\r\n')
        for i in range(70):
            fh.write(f"step {'hijklmn'[i % 7] * (1 + i % 5)} ok\r\n")
    f, _ = c.ok("spawn", {"command": "cmd.exe /d /c type long.txt", "cwd": h.dir})
    sid = f["session"]
    c.ok("wait", {"session": sid, "until": {"exit": True}, "timeout_ms": 60000})
    facts, body = c.ok("read", {"session": sid})
    print("MEASURE", json.dumps({"what": "141 lines, one of 5,090 characters with its error in the middle", "raw_bytes": facts["raw_bytes"], "lean_bytes": facts["lean_bytes"],
                                 "lines": facts["lines"], "lines_shown": facts["lines_shown"], "errors": facts["errors"]}))
    note("the long line is counted as an error line", facts["errors"] == 1, facts)
    note("the error itself is in the lean text", "error: disk full on /dev/sda1" in body, [l[:80] + " ... " + l[-60:] for l in body.split("\n") if len(l) > 300])

    # ---- 3. more rows than the emulator keeps, from a few bytes --------------------
    rep = os.path.join(h.dir, "rep.bin")
    with open(rep, "wb") as fh:
        fh.write(b"start of the run\r\nerror: printed before the flood\r\n")
        fh.write(b"a\x1b[65535b" * 500)          # 32.8 million characters from 4,500 bytes
        fh.write(b"\r\nend of the run\r\n")
    f, _ = c.ok("spawn", {"command": PY, "args": ["-c", f"import sys;sys.stdout.buffer.write(open(r'{rep}','rb').read());sys.stdout.buffer.flush()"], "cols": 120, "rows": 30})
    r = f["session"]
    w, _ = c.ok("wait", {"session": r, "until": {"exit": True}, "timeout_ms": 300000}, timeout=320)
    t = time.perf_counter()
    res, facts, body = c.call("read", {"session": r, "max_bytes": 1 << 20}, timeout=600)
    ms = (time.perf_counter() - t) * 1000
    if res is None or not isinstance(facts, dict):
        note("repeat flood: read answered", False, str(facts)[:300])
    else:
        print("MEASURE", json.dumps({"what": "ESC[65535b 500 times after an error line", "raw_bytes": facts.get("raw_bytes"), "lines": facts.get("lines"),
                                     "errors": facts.get("errors"), "read_ms": round(ms), "server_mb": [round(x) for x in rss_mb(c.p.pid)]}))
        note("repeat flood: the error line printed before it is still reported", "error: printed before the flood" in body, (facts.get("errors"), body[:200]))
    note("server alive after the repeat flood", c.alive())

    # ---- 4. a second server looks all the time while 20 MB floods through ----------
    big = os.path.join(h.dir, "big.txt")
    with open(big, "w", newline="") as fh:
        i = 0
        while fh.tell() < 20 << 20:
            fh.write(f"{i:07} \x1b[3{1 + i % 6}mthe quick brown fox jumps over the lazy dog\x1b[0m 0123456789\r\n")
            if i == 150_000:
                fh.write("error: one bad line in the middle of the flood\r\n")
            i += 1
    guest = Client(h); guest.handshake()
    f, _ = c.ok("spawn", {"command": "cmd.exe /d /c type big.txt", "cwd": h.dir})
    b = f["session"]
    stop = threading.Event()
    seen = {"rounds": 0, "errors": [], "back": 0, "slowest_ms": 0.0}

    def look():
        last = 0
        cursor = None
        while not stop.is_set():
            for tool, args in (("screen", {"session": b}), ("read", {"session": b, "mode": "raw", "max_bytes": 4000} | ({"cursor": cursor} if cursor is not None else {})),
                               ("wait", {"session": b, "until": {"regex": "never-printed-xyz"}, "timeout_ms": 30})):
                t = time.perf_counter()
                res, facts, _ = guest.call(tool, args, timeout=120)
                seen["slowest_ms"] = max(seen["slowest_ms"], (time.perf_counter() - t) * 1000)
                if res is None or res.get("isError") or not isinstance(facts, dict):
                    seen["errors"].append((tool, str(facts)[:200]))
                    continue
                # screen's cursor is the end of the output it showed; read's is where its page ended. Neither may go back.
                # (wait's says how far its search got in 30 ms, from the same start every time: that one may.)
                if tool == "read":
                    seen["back"] += facts["cursor"] < (cursor or 0)
                    cursor = facts["cursor"]
                elif tool == "screen":
                    seen["back"] += facts["cursor"] < last
                    last = facts["cursor"]
            seen["rounds"] += 1

    th = threading.Thread(target=look, daemon=True); th.start()
    t = time.perf_counter()
    w, _ = c.ok("wait", {"session": b, "until": {"exit": True}, "timeout_ms": 300000}, timeout=320)
    flood_s = time.perf_counter() - t
    stop.set(); th.join(60)
    note("flood ran to its exit while a second server looked", w["reason"] == "exit", w)
    note("no call of the second server failed and no cursor went back", not seen["errors"] and seen["back"] == 0, seen)
    total = len(journal_output(f["journal"]))
    for name, cl in (("the server that spawned it", c), ("the second server", guest)):
        t = time.perf_counter()
        facts, body = cl.ok("read", {"session": b, "cursor": 0}, timeout=300)
        ms = (time.perf_counter() - t) * 1000
        note(f"{name}: lean from the top keeps the one error line and consumes every byte", "error: one bad line in the middle of the flood" in body and facts["raw_bytes"] == total and facts["errors"] == 1,
             {k: facts[k] for k in ("raw_bytes", "lean_bytes", "lines", "lines_shown", "errors")} | {"journal": total, "ms": round(ms)})
    print("MEASURE", json.dumps({"what": "20 MB flood with a second server looking", "bytes": total, "flood_s": round(flood_s, 1), "mb_per_s": round(total / 1e6 / flood_s, 2),
                                 "rounds": seen["rounds"], "slowest_call_ms": round(seen["slowest_ms"])}))
    guest.close()

    # ---- 2. a message that never ends its line --------------------------------------
    before = rss_mb(c.p.pid)[0]
    chunk = b"x" * (1 << 20)
    peak = before
    t = time.perf_counter()
    sent = 0
    try:
        for i in range(200):
            c.raw(chunk)
            sent += len(chunk)
            if i % 10 == 9:
                peak = max(peak, rss_mb(c.p.pid)[0])
    except OSError as e:
        note("endless message: the server kept reading", False, f"pipe broke after {sent >> 20} MB: {e}")
    time.sleep(0.3)
    peak = max(peak, rss_mb(c.p.pid)[0])
    c.raw(b"\n")
    bad = c.other(30)
    after = rss_mb(c.p.pid)[0]
    print("MEASURE", json.dumps({"what": "200 MB on standard input with no line end", "server_mb_before": round(before), "server_mb_peak": round(peak), "server_mb_after": round(after),
                                 "write_s": round(time.perf_counter() - t, 1), "answer": (bad or {}).get("error")}))
    note("endless message: answered with a parse error", bool(bad) and bad.get("error", {}).get("code") == -32700, bad)
    note("endless message: the server did not hold it", peak - before < 100, f"{before:.0f} MB before, {peak:.0f} MB at its peak")
    r = c.request("ping", {}, timeout=20)
    note("endless message: the server carries on", r is not None and r.get("result") == {}, r)
    f2, _ = c.ok("sessions", {})
    note("endless message: its sessions are still known", len(f2["ended"]) >= 3, f2["ended"])
    c.close()
finally:
    h.close()
print("\nFINDINGS:", json.dumps(findings, indent=1))
