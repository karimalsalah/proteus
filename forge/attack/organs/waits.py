"""wait: timeouts early/late, exit while waiting, hostile regexes, concurrency."""
import sys, time, json, os, threading
from mcpc import *

h = Home("waits")
findings = []


def note(tag, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + tag + (": " + str(detail)[:500] if detail != "" else ""))
    if not ok:
        findings.append((tag, str(detail)[:700]))


try:
    owner = Client(h); owner.handshake()
    guest = Client(h); guest.handshake()
    f, _ = owner.ok("spawn", {"command": "cmd.exe /d", "cols": 100, "rows": 30, "cwd": h.dir})
    sid = f["session"]
    owner.ok("wait", {"session": sid, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
    owner.ok("wait", {"session": sid, "until": {"quiet_ms": 500}, "timeout_ms": 20000})
    guest.ok("screen", {"session": sid})

    # ---- timeouts: early or late? -------------------------------------------
    for who, c in (("owner", owner), ("guest", guest)):
        rows = []
        worst_late, early = 0.0, 0
        for T in [0, 1, 5, 20, 50, 100, 249, 250, 251, 400, 1000, 2500]:
            for rep in range(3):
                t = time.perf_counter()
                f, _ = c.ok("wait", {"session": sid, "until": {"regex": "never-printed-xyz"}, "timeout_ms": T})
                wall = (time.perf_counter() - t) * 1000
                assert f["reason"] == "timeout", f
                if f["elapsed_ms"] < T:
                    early += 1
                worst_late = max(worst_late, wall - T)
                rows.append((T, round(wall, 1), f["elapsed_ms"]))
        print(who, "timeouts (asked, wall ms, reported):", rows)
        note(f"{who}: no wait returned before its timeout", early == 0, early)
        # How late depends on the machine as much as on the server: lateness.py tells the two apart.
        print("MEASURE", json.dumps({"what": "latest return after a timeout, ms", "who": who, "ms": round(worst_late, 1)}))
        # quiet
        for q in [50, 200, 700]:
            t = time.perf_counter()
            f, _ = c.ok("wait", {"session": sid, "until": {"quiet_ms": q}, "timeout_ms": 10000})
            wall = (time.perf_counter() - t) * 1000
            note(f"{who}: quiet_ms {q} takes at least {q} ms", f["reason"] == "quiet" and wall >= q and wall < q + 400, f"{wall:.1f} ms reported {f['elapsed_ms']}")

    # quiet while output keeps coming: must not return until it stops
    owner.ok("send", {"session": sid, "text": "for /L %i in (1,1,30) do @(echo tick %i & ping -n 1 -w 100 127.0.0.1 >nul)", "enter": True})
    t = time.perf_counter()
    f, _ = owner.ok("wait", {"session": sid, "until": {"quiet_ms": 600}, "timeout_ms": 30000})
    wall = (time.perf_counter() - t) * 1000
    _, scr = owner.ok("screen", {"session": sid})
    note("quiet is not declared while ticks keep coming", "tick 30" in scr and f["reason"] == "quiet", f"{wall:.0f} ms; last rows {scr[-80:]!r}")

    # ---- a regex that matches across a tentative end -------------------------
    f, _ = owner.ok("send", {"session": sid, "text": "echo MARK-ONE & ping -n 2 127.0.0.1 >nul & echo MARK-TWO", "enter": True})
    w, _ = owner.ok("wait", {"session": sid, "until": {"regex": r"^MARK-TWO"}, "timeout_ms": 20000})
    note("regex found after a pause", w["reason"] == "regex" and w["elapsed_ms"] >= 800, w)
    # the default cursor for the next wait is after that match: the same pattern must not match again
    w2, _ = owner.ok("wait", {"session": sid, "until": {"regex": r"^MARK-TWO"}, "timeout_ms": 300})
    note("a matched line is not matched twice by the next wait", w2["reason"] == "timeout", w2)
    # but with the cursor of the send it is found again
    w3, _ = owner.ok("wait", {"session": sid, "until": {"regex": r"^MARK-ONE"}, "timeout_ms": 300, "cursor": f["cursor"]})
    note("with the send cursor it is found again", w3["reason"] == "regex", w3)

    # ---- hostile regexes ------------------------------------------------------
    # something to chew on: 2 MB of text that almost matches
    big = os.path.join(h.dir, "chew.txt")
    with open(big, "w", newline="") as fh:
        line = ("a" * 90 + " ") * 1 + "\r\n"
        while fh.tell() < (2 << 20):
            fh.write(line)
        fh.write("b\r\n")
    f, _ = owner.ok("send", {"session": sid, "text": "type chew.txt & echo CHEW-DONE", "enter": True})
    chew_cursor = f["cursor"]
    w, _ = owner.ok("wait", {"session": sid, "until": {"regex": r"^CHEW-DONE"}, "timeout_ms": 60000})
    note("flood finished", w["reason"] == "regex", w)
    patterns = [
        r"(a+)+$\nZZZ",
        r"(a+)+b",
        r"^(a|aa)+ \nb$",
        r"(a*)*(a*)*(a*)*c",
        r"(?:a?){40}a{40}Q",
        r"([a ]+)+\n([a ]+)+\nQ",
        r"(?s)(.*)(.*)(.*)(.*)(.*)(.*)(.*)(.*)(.*)Q",
        r"\b(\w+\s+)+\1\1Q" if False else r"(\w+\s?)+Q",
        r"[a-z]{1,500}[a-z]{1,500}[a-z]{1,500}Q",
        r"(?:(?:(?:a{1,30}){1,30}){1,30})Q",
    ]
    for pat in patterns:
        for T in (300,):
            t = time.perf_counter()
            res, facts, _ = owner.call("wait", {"session": sid, "until": {"regex": pat}, "timeout_ms": T, "cursor": chew_cursor}, timeout=180)
            wall = (time.perf_counter() - t) * 1000
            if res is None:
                note(f"regex {pat!r}: answered", False, "no answer in 180 s")
                continue
            reason = facts.get("reason") if isinstance(facts, dict) else str(facts)[:80]
            note(f"regex {pat[:40]!r} over 2 MB: timeout {T} honoured within 1 s", wall < T + 1000, f"wall {wall:.0f} ms, reason {reason}")
    # patterns that must be refused quickly
    for pat in [r"a{1000}{1000}{1000}", r"(?:\d{1000}){1000}", "(" * 5000 + ")" * 5000, "x" * (1 << 20), r"\p{L}{50000}", r"(?i)" + "[a-z]{500}" * 200]:
        t = time.perf_counter()
        res, facts, _ = owner.call("wait", {"session": sid, "until": {"regex": pat}, "timeout_ms": 200, "cursor": chew_cursor}, timeout=180)
        wall = (time.perf_counter() - t) * 1000
        ok = res is not None and wall < 3000
        note(f"huge regex {pat[:30]!r}.. ({len(pat)} chars) answered within 3 s", ok, f"wall {wall:.0f} ms isError={res and res.get('isError')} {str(facts)[:100]}")
    note("server alive after regex attacks", owner.alive(), rss_mb(owner.p.pid))

    # ---- concurrency ----------------------------------------------------------
    ids = []
    t = time.perf_counter()
    for i in range(30):
        ids.append(owner.start_call("screen", {"session": sid}))
        ids.append(owner.start_call("read", {"session": sid, "cursor": 0, "max_bytes": 2000}))
        ids.append(owner.start_call("read", {"session": sid, "cursor": chew_cursor, "mode": "plain", "lines": [1, 3]}))
        ids.append(owner.start_call("wait", {"session": sid, "until": {"regex": "never-xyz"}, "timeout_ms": 500}))
        ids.append(owner.start_call("sessions", {}))
        ids.append(owner.start_call("read", {"session": sid, "mode": "raw", "cursor": i * 1000, "max_bytes": 1000}))
        ids.append(guest.start_call("screen", {"session": sid}))
        ids.append(guest.start_call("read", {"session": sid, "cursor": 0, "max_bytes": 2000}))
        ids.append(guest.start_call("wait", {"session": sid, "until": {"regex": "never-xyz"}, "timeout_ms": 500}))
    missing = 0
    errs = 0
    for n, id in enumerate(ids):
        c = guest if n % 9 >= 6 else owner
        res, facts, _ = c.finish(id, 60)
        if res is None:
            missing += 1
        elif res.get("isError"):
            errs += 1
    note(f"{len(ids)} concurrent calls all answered without error", missing == 0 and errs == 0, f"missing={missing} errors={errs} in {(time.perf_counter()-t)*1000:.0f} ms; bad lines {owner.bad[:2]} {guest.bad[:2]}")
    # two shots at once and a send
    ids = [owner.start_call("shot", {"session": sid}) for _ in range(6)]
    oks = [owner.finish(i, 60)[0] for i in ids]
    note("6 concurrent shots", all(o is not None and o.get("isError") is False for o in oks))
    # duplicate ids in flight
    a = owner.start_call("wait", {"session": sid, "until": {"regex": "never-xyz"}, "timeout_ms": 400}, id="dup")
    b = owner.start_call("wait", {"session": sid, "until": {"regex": "never-xyz"}, "timeout_ms": 400}, id="dup")
    r1 = owner.answer("dup", 5); r2 = owner.answer("dup", 5)
    note("two requests with the same id are both answered", r1 is not None and r2 is not None)

    # ---- exit while waiting ---------------------------------------------------
    f, _ = owner.ok("spawn", {"command": "cmd.exe /d"})
    s2 = f["session"]
    owner.ok("wait", {"session": s2, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
    guest.ok("screen", {"session": s2})
    wo = owner.start_call("wait", {"session": s2, "until": {"regex": "never-xyz"}, "timeout_ms": 30000})
    wq = owner.start_call("wait", {"session": s2, "until": {"quiet_ms": 20000}, "timeout_ms": 30000})
    wg = guest.start_call("wait", {"session": s2, "until": {"regex": "never-xyz"}, "timeout_ms": 30000})
    time.sleep(0.6)
    t = time.perf_counter()
    owner.ok("send", {"session": s2, "text": "echo last words & exit 7", "enter": True})
    for name, c, id in (("owner regex", owner, wo), ("owner quiet", owner, wq), ("guest regex", guest, wg)):
        res, facts, _ = c.finish(id, 20)
        wall = (time.perf_counter() - t) * 1000
        note(f"exit while waiting ({name}): reason exit, code 7", facts and facts.get("reason") == "exit" and facts.get("exit_code") == 7, f"{facts} after {wall:.0f} ms")
    # the output printed just before the exit is all there
    for name, c in (("owner", owner), ("guest", guest)):
        _, txt = c.ok("read", {"session": s2, "cursor": 0, "mode": "plain"})
        note(f"{name}: output before the exit is complete", "last words" in txt.split("exit 7")[-1], txt[-120:])
    # waiting on something that has already ended
    for name, c in (("owner", owner), ("guest", guest)):
        t = time.perf_counter()
        f, _ = c.ok("wait", {"session": s2, "until": {"regex": "never-xyz"}, "timeout_ms": 5000})
        note(f"{name}: wait on an ended session returns at once", f["reason"] == "exit" and (time.perf_counter() - t) < 0.5, f)
        f, _ = c.ok("wait", {"session": s2, "until": {"regex": "last words"}, "timeout_ms": 5000, "cursor": 0})
        note(f"{name}: a pattern in an ended session's output is still found", f["reason"] == "regex" and f.get("exited") is True, f)

    # many quick exits: is the last output ever lost, or the exit code wrong?
    lost = 0; wrong = 0; slow = []
    for i in range(25):
        f, _ = owner.ok("spawn", {"command": "cmd.exe", "args": ["/d", "/c", f"echo quick-{i}& exit {i}"]})
        q = f["session"]
        t = time.perf_counter()
        w, _ = owner.ok("wait", {"session": q, "until": {"exit": True}, "timeout_ms": 30000})
        slow.append((time.perf_counter() - t) * 1000)
        _, txt = owner.ok("read", {"session": q})
        if f"quick-{i}" not in txt:
            lost += 1
        if w.get("exit_code") != i:
            wrong += 1
    note("25 quick commands: output never lost, exit code always right", lost == 0 and wrong == 0, f"lost={lost} wrong={wrong} spawn-to-exit ms {summary(slow)}")

    # kill while someone waits
    f, _ = owner.ok("spawn", {"command": "cmd.exe /d"})
    s3 = f["session"]
    owner.ok("wait", {"session": s3, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
    wg = guest.start_call("wait", {"session": s3, "until": {"exit": True}, "timeout_ms": 30000})
    wo = owner.start_call("wait", {"session": s3, "until": {"exit": True}, "timeout_ms": 30000})
    time.sleep(0.5)
    k, _ = owner.ok("kill", {"session": s3})
    r1 = owner.finish(wo, 10)[1]; r2 = guest.finish(wg, 10)[1]
    note("kill ends both waits", r1 and r1.get("reason") == "exit" and r2 and r2.get("reason") == "exit", f"kill={k} owner={r1} guest={r2}")
    k2 = owner.call("kill", {"session": s3})
    note("killing twice is refused cleanly", k2[0] is not None and k2[0].get("isError") is True, k2[1])
    print("sessions:", json.dumps(owner.ok("sessions", {})[0])[:500])
    owner.close(); guest.close()
finally:
    h.close()
print("\nFINDINGS:", json.dumps(findings, indent=1))
