"""Lean against raw on captures made here, checked against the journal with an independent parser;
memory of screen; wait timeout lateness; wake-up latency measured by this client."""
import sys, time, json, os, re, random, subprocess, ctypes
from mcpc import *

PY = sys.executable
h = Home("captures")
findings = []
what = sys.argv[1:] or ["lean", "mem", "late", "wake"]


def note(tag, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + tag + (": " + str(detail)[:700] if detail != "" else ""))
    if not ok:
        findings.append((tag, str(detail)[:900]))


ANSI = re.compile(rb"\x1b\[[0-9;:?<>=!]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[PX^_].*?\x1b\\|\x1b[@-Z\\-_]|\x1b[ -/]*[0-~]", re.S)
ERR = re.compile(r"(?<![A-Za-z])(error|err!|fatal|panic|fail|exception|traceback|denied|abort|rejected|conflict|invalid|unexpected|missing|not found|not recognized|no such|cannot|could not|unable to|segmentation fault|timed out|refused|unresolved|undefined|warn|deprecat)", re.I)


def drain(c, sid, mode, cursor, max_bytes):
    pages = []
    t = time.perf_counter()
    while True:
        a = {"session": sid, "mode": mode, "max_bytes": max_bytes}
        if cursor is not None:
            a["cursor"] = cursor
        f, body = c.ok("read", a, timeout=300)
        pages.append((f, body))
        cursor = f["cursor"]
        if f["more_bytes"] == 0:
            break
    return pages, (time.perf_counter() - t) * 1000


try:
    owner = Client(h); owner.handshake()

    if "lean" in what:
        # 1. cargo build of a small crate written here: warnings of several kinds and one error
        krate = os.path.join(h.dir, "revcrate")
        os.makedirs(os.path.join(krate, "src"))
        open(os.path.join(krate, "Cargo.toml"), "w").write('[package]\nname = "revcrate"\nversion = "0.1.0"\nedition = "2021"\n')
        src = "use std::fmt::Debug;\nuse std::collections::BTreeMap;\n\n"
        for i in range(18):
            src += f"pub fn unit_{i}(input: u64) -> u64 {{\n    let unused_{i} = input * {i};\n    let mut needless = input;\n    needless + {i}\n}}\n\n"
        src += "fn never_called() {}\n\nfn wrong() -> String {\n    let n: usize = 5;\n    n\n}\n\nfn main() {\n    println!(\"{}\", wrong());\n}\n"
        open(os.path.join(krate, "src", "main.rs"), "w").write(src)
        git_dir = WORKTREE
        bar = os.path.join(h.dir, "bar.py")
        open(bar, "w").write(
            "import sys,time\n"
            "for i in range(1,401):\n"
            "    n=i*50//400\n"
            "    sys.stdout.write('\\r\\x1b[2K\\x1b[36m[' + '='*n + '>' + ' '*(50-n) + f']\\x1b[0m {i*100//400:3d}% {i}/400 files  {i*37%997:4d} kB/s')\n"
            "    sys.stdout.flush(); time.sleep(0.004)\n"
            "    if i in (120, 300): sys.stdout.write('\\r\\x1b[2Kwarning: mirror %d was slow, retrying\\n' % i)\n"
            "sys.stdout.write('\\nfetched 400 files\\n')\n")
        cases = [
            ("cargo build --color always (own crate: 18 fns with 2 warnings each, dead code, 1 type error)", "cargo", ["build", "--color", "always"], krate),
            ("git --no-pager log -p --color=always (this repository at HEAD)", "git", ["--no-pager", "log", "-p", "--color=always"], git_dir),
            ("python: a 400-step progress bar with \\r and erase-line, two warnings in between", PY, [bar], h.dir),
        ]
        for name, cmd, args, cwd in cases:
            f, _ = owner.ok("spawn", {"command": cmd, "args": args, "cwd": cwd})
            sid = f["session"]; j = f["journal"]
            w, _ = owner.ok("wait", {"session": sid, "until": {"exit": True}, "timeout_ms": 300000}, timeout=320)
            assert w["reason"] == "exit", w
            lean, lms = drain(owner, sid, "lean", None, 24000)
            plain, pms = drain(owner, sid, "plain", 0, 1 << 20)
            rawp, rms = drain(owner, sid, "raw", 0, 1 << 20)
            raw_journal = journal_output(j)
            raw_reported = sum(f["raw_bytes"] for f, _ in lean)
            lean_text = "\n".join(b for _, b in lean)
            lean_bytes = sum(f["lean_bytes"] for f, _ in lean)
            plain_text = "\n".join(b for _, b in plain)
            raw_text = "".join(b for _, b in rawp)
            row = {"capture": name, "exit_code": w.get("exit_code"), "raw_bytes_reported": raw_reported, "raw_bytes_in_journal": len(raw_journal),
                   "lean_bytes": lean_bytes, "lean_bytes_counted_here": sum(len(b.encode()) for _, b in lean), "saved_pct": round((1 - lean_bytes / max(1, raw_reported)) * 100, 1),
                   "reads": len(lean), "lines": sum(f["lines"] for f, _ in lean), "lines_shown": sum(f["lines_shown"] for f, _ in lean),
                   "errors": sum(f["errors"] for f, _ in lean), "warnings": sum(f["warnings"] for f, _ in lean), "plain_bytes": sum(f["lean_bytes"] for f, _ in plain), "lean_ms": round(lms), "plain_ms": round(pms)}
            print("MEASURE", json.dumps(row))
            note(f"{cmd}: raw_bytes equals the journal's output bytes", raw_reported == len(raw_journal), (raw_reported, len(raw_journal)))
            note(f"{cmd}: lean_bytes is the size of the text returned", lean_bytes == row["lean_bytes_counted_here"], (lean_bytes, row["lean_bytes_counted_here"]))
            note(f"{cmd}: raw mode returns the journal bytes", raw_text.encode("utf-8", "replace") == raw_journal.decode("utf-8", "replace").encode("utf-8", "replace"), (len(raw_text.encode()), len(raw_journal)))
            # independent check 1: every plain line that looks like an error or warning is in the lean text
            kept = set(x.strip() for x in lean_text.split("\n"))
            sig = [l.strip() for l in plain_text.split("\n") if ERR.search(l)]
            lost = [l for l in sig if l not in kept and len(l) <= 1000]
            note(f"{cmd}: every error/warning line of the plain text is in the lean text", not lost, f"{len(sig)} signal lines, lost {len(lost)}: {lost[:3]}")
            # independent check 2 (line-oriented output only): the same judged from the raw journal stripped with a regex written here
            if cmd != "git":
                stripped = ANSI.sub(b"", raw_journal).decode("utf-8", "replace")
                final = [seg.split("\r")[-1] for seg in stripped.replace("\r\n", "\n").split("\n")]
                sig2 = [l.strip() for l in final if ERR.search(l) and l.strip()]
                lost2 = [l for l in sig2 if l not in kept]
                note(f"{cmd}: every error/warning line of the stripped journal is in the lean text", not lost2, f"{len(sig2)} signal lines, lost {len(lost2)}: {lost2[:3]}")
            if cmd == "cargo":
                print("---- lean text of the cargo build (first 25 and last 12 lines) ----")
                ls = lean_text.split("\n")
                print("\n".join(ls[:25])); print("  ..."); print("\n".join(ls[-12:]))
            if cmd == PY:
                print("---- lean text of the progress bar ----"); print(lean_text)

    if "mem" in what:
        # memory held by `screen` after a long session, 120 and 500 columns
        text = os.path.join(h.dir, "long.txt")
        with open(text, "w", newline="") as fh:
            i = 0
            while fh.tell() < 20 * (1 << 20):
                fh.write(f"{i:07} \x1b[3{1 + i % 6}mthe quick brown fox jumps over the lazy dog\x1b[0m 0123456789 abcdefghijklmnopqrstuvwxyz\r\n")
                i += 1
        for cols in (120, 500):
            c = Client(h); c.handshake()
            f, _ = c.ok("spawn", {"command": "cmd.exe /d", "cwd": h.dir, "cols": cols, "rows": 32})
            sid = f["session"]
            c.ok("wait", {"session": sid, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
            c.ok("send", {"session": sid, "text": "type long.txt & echo LONG-OVER", "enter": True})
            c.ok("wait", {"session": sid, "until": {"regex": "^LONG-OVER"}, "timeout_ms": 600000}, timeout=700)
            c.ok("wait", {"session": sid, "until": {"quiet_ms": 400}, "timeout_ms": 20000})
            g = Client(h); g.handshake()
            g.ok("sessions", {})
            r0, g0 = rss_mb(c.p.pid)[0], rss_mb(g.p.pid)[0]
            t = time.perf_counter(); c.ok("screen", {"session": sid}); ms1 = (time.perf_counter() - t) * 1000
            t = time.perf_counter(); g.ok("screen", {"session": sid}); ms2 = (time.perf_counter() - t) * 1000
            r1, g1 = rss_mb(c.p.pid)[0], rss_mb(g.p.pid)[0]
            c.ok("read", {"session": sid, "cursor": 0}, timeout=300); g.ok("read", {"session": sid, "cursor": 0}, timeout=300)
            r2, g2 = rss_mb(c.p.pid), rss_mb(g.p.pid)
            print("MEASURE", json.dumps({"what": "memory held after screen on 20 MB of history", "cols": cols, "owner_rss_mb_before": round(r0, 1), "owner_rss_mb_after_screen": round(r1, 1),
                                         "guest_rss_mb_before": round(g0, 1), "guest_rss_mb_after_screen": round(g1, 1), "screen_ms": [round(ms1), round(ms2)],
                                         "owner_rss_peak_after_lean_read": [round(x, 1) for x in r2], "guest_rss_peak_after_lean_read": [round(x, 1) for x in g2]}))
            c.ok("kill", {"session": sid}); c.close(); g.close()

    if "late" in what:
        f, _ = owner.ok("spawn", {"command": "cmd.exe /d"})
        sid = f["session"]
        owner.ok("wait", {"session": sid, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
        owner.ok("wait", {"session": sid, "until": {"quiet_ms": 500}, "timeout_ms": 20000})
        guest = Client(h); guest.handshake(); guest.ok("screen", {"session": sid})
        for who, c in (("spawned here", owner), ("another server's", guest), ("spawned here", owner), ("another server's", guest)):
            for T in (100, 249):
                late = []; inner = []
                for _ in range(60):
                    t = time.perf_counter()
                    f, _ = c.ok("wait", {"session": sid, "until": {"regex": "never-printed-xyz"}, "timeout_ms": T})
                    late.append((time.perf_counter() - t) * 1000 - T); inner.append(f["elapsed_ms"] - T)
                print("MEASURE", json.dumps({"what": "wait timeout lateness, ms past the timeout", "session": who, "timeout_ms": T, "as_the_client_sees_it": summary(late), "as_the_server_reports_it": summary(inner)}))
        guest.close()

    if "wake" in what:
        # wake-up latency with this client's own clock: a python ticker prints time.time_ns()
        tick = os.path.join(h.dir, "tick.py")
        open(tick, "w").write("import sys,time\nfor i in range(41):\n    time.sleep(0.25)\n    sys.stdout.write('TICK %d\\n' % time.time_ns()); sys.stdout.flush()\n")
        for who in ("spawned here", "another server's"):
            f, _ = owner.ok("spawn", {"command": PY, "args": [tick]})
            sid = f["session"]
            g = Client(h); g.handshake()
            c = owner if who == "spawned here" else g
            ms = []; cur = 0
            while True:
                w, _ = c.ok("wait", {"session": sid, "until": {"regex": r"^TICK (\d+)"}, "timeout_ms": 30000, "cursor": cur})
                now = time.time_ns()
                if w["reason"] != "regex":
                    break
                cur = w["cursor"]
                ms.append((now - int(w["groups"][0])) / 1e6)
            print("MEASURE", json.dumps({"what": "wait wake-up, ms from the program's own clock at print to this client having the answer", "session": who, "ms": summary(ms[1:])}))
            g.close()
    owner.close()
finally:
    h.close()
print("\nFINDINGS:", json.dumps(findings, indent=1))
