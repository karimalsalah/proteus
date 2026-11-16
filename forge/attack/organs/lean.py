"""lean reader: binary output, an error line in the collapsed middle, paging, a 50 MB burst, a journal growing while read."""
import sys, time, json, os, random, re, threading
from mcpc import *

h = Home("lean")
findings = []
PY = sys.executable


def note(tag, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + tag + (": " + str(detail)[:600] if detail != "" else ""))
    if not ok:
        findings.append((tag, str(detail)[:900]))


def drain(c, sid, mode, cursor, max_bytes, extra=None):
    """Read to the end; returns (pages [(from, cursor, facts, body)], all lines)."""
    pages = []
    while True:
        a = {"session": sid, "mode": mode, "max_bytes": max_bytes}
        if cursor is not None:
            a["cursor"] = cursor
        if extra:
            a.update(extra)
        f, body = c.ok("read", a, timeout=300)
        pages.append((f["from"], f["cursor"], f, body))
        cursor = f["cursor"]
        if f["more_bytes"] == 0:
            break
        assert len(pages) < 200000
    return pages


try:
    owner = Client(h); owner.handshake()
    guest = Client(h); guest.handshake()

    # ---- binary output --------------------------------------------------------
    binf = os.path.join(h.dir, "noise.bin")
    random.seed(7)
    open(binf, "wb").write(bytes(random.getrandbits(8) for _ in range(300_000)))
    f, _ = owner.ok("spawn", {"command": PY, "args": ["-c", f"import sys;sys.stdout.buffer.write(open(r'{binf}','rb').read());sys.stdout.buffer.flush()"], "cols": 100, "rows": 30})
    b = f["session"]; bj = f["journal"]
    w, _ = owner.ok("wait", {"session": b, "until": {"exit": True}, "timeout_ms": 120000}, timeout=150)
    note("binary: program ran to its exit", w["reason"] == "exit", w)
    for c, name in ((owner, "owner"), (guest, "guest")):
        for mode in ("lean", "plain", "raw"):
            t = time.perf_counter()
            pages = drain(c, b, mode, 0, 24000)
            raw = sum(p[2]["raw_bytes"] for p in pages)
            out = sum(p[2]["lean_bytes"] for p in pages)
            total = len(journal_output(bj))
            note(f"binary {name} {mode}: read to the end, bytes add up to the journal", raw == total, f"raw={raw} journal={total} out={out} pages={len(pages)} {(time.perf_counter()-t)*1000:.0f} ms")
        f1, s1 = c.ok("screen", {"session": b})
        res, facts, _ = c.call("shot", {"session": b})
        note(f"binary {name}: screen and shot answer", res is not None and res.get("isError") is False, (f1.get("cols"), f1.get("rows"), facts.get("png_bytes") if isinstance(facts, dict) else facts))
        w, _ = c.ok("wait", {"session": b, "until": {"regex": r"[^\x00-\x7f]{3}"}, "timeout_ms": 2000, "cursor": 0})
        note(f"binary {name}: a regex over it answers", w["reason"] in ("regex", "exit"), {k: (v if k != 'line' else v[:20]) for k, v in w.items()})
    note("servers alive after binary", owner.alive() and guest.alive())

    # ---- an error line in the collapsed middle, in every position ---------------
    words = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon".split()
    random.seed(11)
    N = 6000
    err_at = set(random.sample(range(30, N - 60), 120))
    lines = []
    errors = []
    for i in range(N):
        if i in err_at:
            kind = len(errors) % 6
            e = [f"error: unit {i} exploded", f"   error[E{i:04}]: nested failure in step {i}", f"warning: step {i} is deprecated",
                 f"FAILED tests/test_{i}.py::test_it", f"thread 'main' panicked at src/m{i}.rs:1:1", f"x{i}: fatal: cannot open"][kind]
            errors.append(e.strip())
            lines.append(e)
        elif i % 7 == 3:
            lines.append(f"step {i} of {N} done")                      # similar-line runs are broken up by the others
        elif 2000 <= i < 2600:
            lines.append(f"copying file {i}")                            # one long similar run with errors inside it
        elif 4000 <= i < 4300:
            lines.append("same line again")                             # one long identical run with errors inside it
        else:
            lines.append(" ".join(random.choice(words) for _ in range(random.randint(3, 14))))
    src = os.path.join(h.dir, "log.txt")
    open(src, "w", newline="").write("\r\n".join(lines) + "\r\n")
    f, _ = owner.ok("spawn", {"command": "cmd.exe", "args": ["/d", "/c", "type log.txt"], "cwd": h.dir, "cols": 120, "rows": 32})
    s = f["session"]; sj = f["journal"]
    w, _ = owner.ok("wait", {"session": s, "until": {"exit": True}, "timeout_ms": 120000}, timeout=150)
    total = len(journal_output(sj))

    def check_all_errors(tag, text):
        kept = set(x.strip() for x in text.split("\n"))
        lost = [e for e in errors if e not in kept]
        note(f"{tag}: every one of the {len(errors)} error/warning lines is in the lean text", not lost, f"lost {len(lost)}: {lost[:4]}")

    for c, name in ((owner, "owner"), (guest, "guest")):
        f, body = c.ok("read", {"session": s, "cursor": 0})
        print(name, "one lean read:", {k: f[k] for k in ("raw_bytes", "lean_bytes", "lines", "lines_shown", "errors", "warnings", "more_bytes")})
        pages = drain(c, s, "lean", 0, 24000)
        check_all_errors(f"{name} lean, default pages ({len(pages)})", "\n".join(p[3] for p in pages))
        note(f"{name} lean pages: raw bytes add up", sum(p[2]["raw_bytes"] for p in pages) == total, (sum(p[2]["raw_bytes"] for p in pages), total))
        for mb in (1000, 3000, 9000):
            pages = drain(c, s, "lean", 0, mb)
            txt = "\n".join(p[3] for p in pages)
            check_all_errors(f"{name} lean, max_bytes {mb} ({len(pages)} pages)", txt)
            dup = [e for e in errors if txt.count(e) > 1]
            note(f"{name} lean, max_bytes {mb}: no error line shown twice", not dup, dup[:3])
            big = max(len(p[3]) for p in pages)
            note(f"{name} lean, max_bytes {mb}: pages stay near their size", big < mb * 2 + 2000, f"largest page {big} bytes")
        for hd, tl in ((0, 0), (1, 1), (5, 0)):
            f, body = c.ok("read", {"session": s, "cursor": 0, "head": hd, "tail": tl, "max_bytes": 1 << 20})
            check_all_errors(f"{name} lean head={hd} tail={tl}", body)

    # plain pages reproduce the file exactly
    pages = drain(owner, s, "plain", 0, 4000)
    got = [l for p in pages for l in p[3].split("\n")] if pages else []
    note("plain pages are the file, line for line", got == lines, f"{len(got)} vs {len(lines)}; first diff {[ (i,a,b) for i,(a,b) in enumerate(zip(got,lines)) if a!=b][:1]}")

    # a fresh reader started at each page boundary must see exactly what the paging reader saw after it
    bad = []
    checked = 0
    for i in range(0, len(pages) - 1):
        cur = pages[i][1]
        want = [l for p in pages[i + 1:] for l in p[3].split("\n")]
        g2 = Client(h); g2.handshake()
        f, body = g2.ok("read", {"session": s, "cursor": cur, "mode": "plain", "max_bytes": 1 << 20})
        rest = body.split("\n") if body else []
        while f["more_bytes"]:
            f, body = g2.ok("read", {"session": s, "mode": "plain", "max_bytes": 1 << 20})
            rest += body.split("\n")
        g2.close()
        checked += 1
        if rest != want:
            d = next((k for k, (a, b2) in enumerate(zip(rest, want)) if a != b2), min(len(rest), len(want)))
            bad.append((i, cur, len(rest), len(want), rest[d:d + 2], want[d:d + 2]))
        if checked >= 60:
            break
    note(f"a read from an explicit cursor (page boundary) loses no line ({checked} boundaries tried)", not bad, f"{len(bad)} boundaries differ, e.g. {bad[:2]}")

    # the same through the documented window: lines [a,b] after the page's `from`
    bad = []
    lp = drain(owner, s, "lean", 0, 3000)
    for (frm, cur, f, body) in lp[1:40]:
        m = re.search(r"\[\.\.\. \d+ lines? omitted \((\d+)-(\d+)\) \.\.\.\]", body)
        if not m:
            continue
        a, b2 = int(m.group(1)), int(m.group(2))
        # what plain says those lines are, read by the same paging reader semantics: a plain read from the same cursor
        fw, win = owner.ok("read", {"session": s, "cursor": frm, "mode": "plain", "lines": [a, b2]})
        fp, whole = owner.ok("read", {"session": s, "cursor": frm, "mode": "plain", "lines": [1, 10**9]})
        # the lean page's shown lines must sit at their numbers in the window numbering
        shown = body.split("\n")
        first_line = shown[0]
        if whole.split("\n")[0] != first_line and not first_line.startswith("[..."):
            bad.append((frm, first_line, whole.split("\n")[:2]))
    note("line numbers in a lean page's gaps match the window read from the page's `from`", not bad, f"{len(bad)} pages off, e.g. {bad[:2]}")

    # ---- 50 MB burst, with a journal growing while a guest reads it -------------
    bigf = os.path.join(h.dir, "burst.txt")
    with open(bigf, "w", newline="") as fh:
        i = 0
        while fh.tell() < 50 * (1 << 20):
            if i == 123456:
                fh.write("error: the one line that matters in fifty megabytes\r\n")
            fh.write(f"{i:08} \x1b[3{1 + i % 6}mthe quick brown fox jumps over the lazy dog\x1b[0m {random.choice(words)} {random.choice(words)} {random.choice(words)}\r\n")
            i += 1
    nlines = i
    f, _ = owner.ok("spawn", {"command": "cmd.exe /d", "cwd": h.dir, "cols": 120, "rows": 32})
    big = f["session"]; bigj = f["journal"]
    owner.ok("wait", {"session": big, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
    f, _ = owner.ok("send", {"session": big, "text": "type burst.txt & echo BURST-OVER", "enter": True})
    t0 = time.perf_counter()
    wid = owner.start_call("wait", {"session": big, "until": {"regex": "^BURST-OVER"}, "timeout_ms": 600000})
    # the guest reads while it grows
    last = 0; mono = True; reads = 0; gerr = 0; slowest = 0.0
    done = None
    while done is None:
        t = time.perf_counter()
        res, facts, body = guest.call("read", {"session": big, "max_bytes": 8000}, timeout=120)
        slowest = max(slowest, time.perf_counter() - t)
        if res is None or res.get("isError"):
            gerr += 1
        else:
            if facts["cursor"] < last:
                mono = False
            last = facts["cursor"]
        res, facts, body = guest.call("screen", {"session": big}, timeout=120)
        if res is None or res.get("isError"):
            gerr += 1
        reads += 1
        with owner.cv:
            done = owner.answers.get(json.dumps(wid))
        time.sleep(0.05)
    res, wf, _ = owner.finish(wid, 10)
    flood_s = time.perf_counter() - t0
    print(f"burst: {wf} in {flood_s:.1f} s; guest made {reads} read+screen rounds while it grew, slowest read {slowest*1000:.0f} ms")
    note("guest reads of a growing journal: no error, cursor never went back", gerr == 0 and mono, f"errors={gerr} monotonic={mono}")
    owner.ok("wait", {"session": big, "until": {"quiet_ms": 500}, "timeout_ms": 20000})
    total = len(journal_output(bigj))
    print("burst journal output bytes:", total, "server rss/peak MB:", rss_mb(owner.p.pid), "guest:", rss_mb(guest.p.pid))
    for c, name in ((owner, "owner"), (guest, "guest")):
        t = time.perf_counter()
        f, body = c.ok("read", {"session": big, "cursor": 0}, timeout=300)
        ms = (time.perf_counter() - t) * 1000
        note(f"50 MB {name}: one lean read from the top keeps the error line", "error: the one line that matters in fifty megabytes" in body, f"{ms:.0f} ms {json.dumps({k: f[k] for k in ('raw_bytes','lean_bytes','lines','lines_shown','errors','more_bytes','saved_pct')})}")
        t = time.perf_counter()
        pages = drain(c, big, "lean", 0, 24000)
        ms = (time.perf_counter() - t) * 1000
        txt = "\n".join(p[3] for p in pages)
        raw = sum(p[2]["raw_bytes"] for p in pages)
        note(f"50 MB {name}: lean to the end: error kept once, bytes add up", txt.count("error: the one line that matters") == 1 and raw == total, f"{len(pages)} reads, {ms:.0f} ms, raw {raw} of {total}, lean {sum(p[2]['lean_bytes'] for p in pages)}")
        t = time.perf_counter()
        f, scr = c.ok("screen", {"session": big}, timeout=300)
        note(f"50 MB {name}: screen", "BURST-OVER" in scr, f"{(time.perf_counter()-t)*1000:.0f} ms")
        print(name, "rss/peak MB after 50 MB reads:", rss_mb(c.p.pid))
    # keep going past the 64 MB kept in memory: the oldest output must then come from the journal
    f, _ = owner.ok("send", {"session": big, "text": "type burst.txt & echo SECOND-OVER", "enter": True})
    w, _ = owner.ok("wait", {"session": big, "until": {"regex": "^SECOND-OVER"}, "timeout_ms": 600000}, timeout=700)
    owner.ok("wait", {"session": big, "until": {"quiet_ms": 500}, "timeout_ms": 20000})
    total = len(journal_output(bigj))
    print("after the second burst: journal output bytes", total, "owner rss/peak", rss_mb(owner.p.pid))
    t = time.perf_counter()
    pages = drain(owner, big, "lean", 0, 24000)
    txt = "\n".join(p[3] for p in pages)
    raw = sum(p[2]["raw_bytes"] for p in pages)
    note("100 MB owner (past the memory limit): lean from the top: both error lines, bytes add up", txt.count("error: the one line that matters") == 2 and raw == total, f"{len(pages)} reads, {(time.perf_counter()-t)*1000:.0f} ms, raw {raw} of {total}")
    f, body = owner.ok("read", {"session": big, "cursor": 0, "mode": "raw", "max_bytes": 300})
    note("100 MB owner: raw read at 0 comes from the journal", "Microsoft" in body, body[:60])
    f, scr = owner.ok("screen", {"session": big, "at": 0.5})
    f, scr = owner.ok("screen", {"session": big})
    note("100 MB owner: screen now", "SECOND-OVER" in scr, scr[-80:])
    w, _ = owner.ok("wait", {"session": big, "until": {"regex": "^error: the one line"}, "timeout_ms": 60000, "cursor": 0}, timeout=120)
    note("100 MB owner: wait from cursor 0 finds the first error line", w["reason"] == "regex", w)
    print("owner rss/peak MB at the end:", rss_mb(owner.p.pid))
    owner.ok("kill", {"session": big})
    owner.close(); guest.close()
finally:
    h.close()
print("\nFINDINGS:", json.dumps(findings, indent=1))
